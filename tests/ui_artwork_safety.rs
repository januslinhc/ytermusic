use std::{
    collections::VecDeque,
    error::Error,
    io::Cursor,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll},
    thread,
    time::{Duration, Instant as StdInstant},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, stream};
use image::{DynamicImage, ImageFormat, RgbaImage};
use url::Url;
use ytermusic::{
    app::Generation,
    domain::MediaId,
    resolver::{PreviewStreamUrl, ResolvedStream},
    ui::{
        animation::{AnimationFrameStore, AnimationKey, AnimationRequest},
        artwork::{
            ARTWORK_LOAD_TIMEOUT, ArtworkByteStream, ArtworkDecoder, ArtworkError,
            ArtworkFetchError, ArtworkFetcher, ArtworkGrid, ArtworkPresentation,
            CachedArtworkService, CellSize, MAX_ENCODED_BYTES, decode_artwork, decode_rgb_frame,
        },
        spectrum::{SpectrumFrameStore, SpectrumKey, SpectrumRequest, SpectrumTarget},
        theme::ColorCapability,
    },
};

type TestResult = Result<(), Box<dyn Error>>;
const TEST_WAIT_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn animation_rgb_frames_obey_exact_output_bounds() {
    let size = CellSize::new(2, 1);
    assert!(decode_rgb_frame(&[0; 12], size).is_ok());
    assert_eq!(
        decode_rgb_frame(&[0; 11], size),
        Err(ArtworkError::DecodeFailed)
    );
    assert_eq!(
        decode_rgb_frame(&[], CellSize::new(0, 1)),
        Err(ArtworkError::OutputResourceLimit)
    );
}

#[test]
fn animation_debug_never_exposes_preview_or_media_identity() -> TestResult {
    let secret_id = "SECRET_MEDIA_ID";
    let secret_query = "SECRET_PREVIEW_TOKEN";
    let key = AnimationKey::new(
        Generation::new(1),
        MediaId {
            provider: "SECRET_PROVIDER".to_owned(),
            video_id: secret_id.to_owned(),
        },
        CellSize::new(2, 1),
    );
    let request = AnimationRequest::new(
        key.clone(),
        PreviewStreamUrl::parse(&format!(
            "https://video.invalid/preview?token={secret_query}"
        ))?,
    );
    let store = AnimationFrameStore::new();
    assert!(store.request(key));
    let debug = format!("{request:?} {store:?}");
    assert!(!debug.contains(secret_id));
    assert!(!debug.contains(secret_query));
    assert!(!debug.contains("SECRET_PROVIDER"));
    Ok(())
}

#[test]
fn animation_grid_debug_redacts_frame_pixel_content() -> TestResult {
    let grid = decode_rgb_frame(&[231; 12], CellSize::new(2, 1))?;
    let debug = format!("{grid:?}");
    assert!(debug.contains("ArtworkGrid"));
    assert!(!debug.contains("231"));
    assert!(!debug.contains("cells"));
    Ok(())
}

#[test]
fn spectrum_runtime_debug_never_exposes_stream_or_media_identity() -> TestResult {
    let secret_id = "SECRET_SPECTRUM_MEDIA";
    let secret_query = "SECRET_SPECTRUM_TOKEN";
    let media_id = MediaId {
        provider: "SECRET_SPECTRUM_PROVIDER".to_owned(),
        video_id: secret_id.to_owned(),
    };
    let stream = ResolvedStream::from_raw_audio_url(
        media_id.clone(),
        &format!("https://audio.invalid/stream?token={secret_query}"),
        time::OffsetDateTime::UNIX_EPOCH,
    )?;
    let target = SpectrumTarget::new(32, 3).ok_or("valid spectrum target")?;
    let key = SpectrumKey::new(Generation::new(1), media_id, target);
    let request = SpectrumRequest::new(
        key.clone(),
        stream.analysis_stream_url().ok_or("analysis URL")?,
    );
    let store = SpectrumFrameStore::new();
    let _run = store.request(key).ok_or("spectrum lease")?;

    let debug = format!("{request:?} {store:?}");
    assert!(!debug.contains(secret_id));
    assert!(!debug.contains(secret_query));
    assert!(!debug.contains("SECRET_SPECTRUM_PROVIDER"));
    Ok(())
}

#[tokio::test]
async fn oversized_chunk_stream_stops_at_the_encoded_cap() -> TestResult {
    let calls = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let chunk_size = MAX_ENCODED_BYTES / 4;
    let chunks = [
        Bytes::from(vec![1; chunk_size]),
        Bytes::from(vec![2; chunk_size]),
        Bytes::from(vec![3; chunk_size]),
        Bytes::from(vec![4; chunk_size]),
        Bytes::from_static(&[5]),
        Bytes::from_static(b"must not be polled"),
    ];
    let fetcher = ChunkFetcher {
        chunks: chunks.into(),
        calls: Arc::clone(&calls),
        polls: Arc::clone(&polls),
    };
    let mut service = CachedArtworkService::new(fetcher, 1);
    let url = Url::parse("https://example.invalid/cover?token=DO_NOT_EXPOSE")?;

    let result = service
        .load(&url, CellSize::new(1, 1), ColorCapability::TrueColor)
        .await;

    assert!(matches!(result, ArtworkPresentation::Fallback(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        polls.load(Ordering::SeqCst),
        5,
        "the stream must not be polled after the first over-limit chunk"
    );
    assert!(!format!("{service:?}").contains("DO_NOT_EXPOSE"));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn stalled_chunk_stream_times_out_under_paused_tokio_time() -> TestResult {
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = PendingFetcher {
        calls: Arc::clone(&calls),
    };
    let mut service = CachedArtworkService::new(fetcher, 1);
    let url = Url::parse("https://example.invalid/stalled?token=DO_NOT_EXPOSE")?;
    let load = tokio::spawn(async move {
        service
            .load(&url, CellSize::new(1, 1), ColorCapability::TrueColor)
            .await
    });

    tokio::task::yield_now().await;
    assert!(!load.is_finished());
    tokio::time::advance(ARTWORK_LOAD_TIMEOUT + Duration::from_millis(1)).await;

    assert!(matches!(load.await?, ArtworkPresentation::Fallback(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn infinite_always_ready_empty_and_tiny_chunks_hit_the_poll_cap() -> TestResult {
    for chunk in [Bytes::new(), Bytes::from_static(&[1])] {
        let polls = run_infinite_ready_stream(chunk, false)?;
        assert!(polls > 0);
        assert!(
            polls <= 100_000,
            "an adversarial ready stream must have a finite poll budget"
        );
    }
    Ok(())
}

#[test]
fn infinite_always_ready_stream_yields_for_the_paused_deadline() -> TestResult {
    let polls = run_infinite_ready_stream(Bytes::new(), true)?;
    assert!(polls > 0);
    assert!(
        polls <= 512,
        "the deadline must be observed after a small cooperative batch"
    );
    Ok(())
}

#[test]
fn infinite_always_ready_stream_observes_cancellation_within_a_batch() -> TestResult {
    let (polls_at_abort, final_polls) = run_infinite_ready_cancellation()?;
    assert!(polls_at_abort > 0);
    assert!(final_polls >= polls_at_abort);
    assert!(
        final_polls - polls_at_abort <= 128,
        "cancellation must stop polling within one small cooperative batch"
    );
    Ok(())
}

#[tokio::test]
async fn output_size_is_validated_before_early_return_or_fetch() -> TestResult {
    let calls = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let fetcher = ChunkFetcher {
        chunks: [Bytes::from_static(b"unused")].into(),
        calls: Arc::clone(&calls),
        polls,
    };
    let mut service = CachedArtworkService::new(fetcher, 1);
    let url = Url::parse("https://example.invalid/size?token=DO_NOT_EXPOSE")?;

    assert!(matches!(
        service
            .load(&url, CellSize::new(0, u16::MAX), ColorCapability::TrueColor,)
            .await,
        ArtworkPresentation::Fallback(_)
    ));

    for size in [CellSize::new(0, 7), CellSize::new(7, 0)] {
        let ArtworkPresentation::Grid(grid) =
            service.load(&url, size, ColorCapability::TrueColor).await
        else {
            panic!("valid zero-area requests should return an empty grid");
        };
        assert_eq!((grid.width(), grid.height()), (size.width, size.height));
        assert!(grid.cells().is_empty());
    }

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "invalid and zero-area requests must not fetch"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_decode_keeps_the_async_runtime_schedulable() -> TestResult {
    let state = Arc::new(BlockingDecodeState::default());
    let _release_all = DecodeReleaseGuard::all(Arc::clone(&state));
    let decoder = BlockingDecoder {
        state: Arc::clone(&state),
    };
    let fetcher = one_image_fetcher(encoded_png()?);
    let mut service = CachedArtworkService::with_decoder(fetcher, 1, decoder);
    let url = Url::parse("https://example.invalid/decode?token=DO_NOT_EXPOSE")?;
    let (heartbeat_tx, heartbeat_rx) = mpsc::sync_channel(1);
    let observer_state = Arc::clone(&state);
    let observer = thread::spawn(move || {
        let _release_first = DecodeReleaseGuard::through(Arc::clone(&observer_state), 1);
        observer_state.wait_for_entered(1, TEST_WAIT_TIMEOUT)?;
        let heartbeat_ran = heartbeat_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        Ok::<_, std::io::Error>(heartbeat_ran)
    });

    let load = tokio::spawn(async move {
        service
            .load(&url, CellSize::new(1, 1), ColorCapability::TrueColor)
            .await
    });
    tokio::task::yield_now().await;
    let _ = heartbeat_tx.send(());

    let load_result = tokio::time::timeout(TEST_WAIT_TIMEOUT, load).await;
    let observer_result = observer
        .join()
        .map_err(|_| std::io::Error::other("decode observer thread panicked"));
    let presentation =
        load_result.map_err(|_| std::io::Error::other("blocking decode load timed out"))??;
    let heartbeat_ran = observer_result??;
    assert!(matches!(presentation, ArtworkPresentation::Grid(_)));
    assert!(
        heartbeat_ran,
        "the current-thread runtime must run a heartbeat while decode is blocked"
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_load_keeps_decode_capacity_until_blocking_work_finishes() -> TestResult {
    let state = Arc::new(BlockingDecodeState::default());
    let _release_all = DecodeReleaseGuard::all(Arc::clone(&state));
    let decoder = BlockingDecoder {
        state: Arc::clone(&state),
    };
    let service = Arc::new(tokio::sync::Mutex::new(CachedArtworkService::with_decoder(
        one_image_fetcher(encoded_png()?),
        1,
        decoder,
    )));
    let url = Url::parse("https://example.invalid/cancel?token=DO_NOT_EXPOSE")?;

    let first_service = Arc::clone(&service);
    let first_url = url.clone();
    let first = tokio::spawn(async move {
        first_service
            .lock()
            .await
            .load(&first_url, CellSize::new(1, 1), ColorCapability::TrueColor)
            .await
    });
    wait_for_decode_count(&state, 1).await?;
    first.abort();
    let first_result = tokio::time::timeout(TEST_WAIT_TIMEOUT, first)
        .await
        .map_err(|_| std::io::Error::other("cancelled decoder waiter did not finish"))?;
    assert!(first_result.is_err());

    let second_service = Arc::clone(&service);
    let second = tokio::spawn(async move {
        second_service
            .lock()
            .await
            .load(&url, CellSize::new(1, 1), ColorCapability::TrueColor)
            .await
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.entered(),
        1,
        "cancelling the waiter must not release the running decoder's permit"
    );

    state.release_through(1);
    wait_for_decode_count(&state, 2).await?;
    assert_eq!(state.max_active(), 1);
    state.release_through(2);

    let presentation = tokio::time::timeout(TEST_WAIT_TIMEOUT, second)
        .await
        .map_err(|_| std::io::Error::other("second decoder load timed out"))??;
    assert!(matches!(presentation, ArtworkPresentation::Grid(_)));
    assert_eq!(state.max_active(), 1);
    Ok(())
}

#[test]
fn blocking_decode_wait_timeout_restores_the_active_counter() {
    let state = BlockingDecodeState::default();

    assert!(state.enter_and_wait(Duration::ZERO).is_err());
    assert_eq!(state.entered(), 1);
    assert_eq!(state.active(), 0);
}

#[test]
fn observer_wait_timeout_returns_without_a_decoder() {
    let state = BlockingDecodeState::default();

    assert!(state.wait_for_entered(1, Duration::ZERO).is_err());
    assert_eq!(state.entered(), 0);
}

#[test]
fn release_guard_unblocks_a_waiting_decoder_on_drop() -> TestResult {
    let state = Arc::new(BlockingDecodeState::default());
    let release = DecodeReleaseGuard::all(Arc::clone(&state));
    let worker_state = Arc::clone(&state);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let _ = result_tx.send(worker_state.enter_and_wait(Duration::from_secs(1)).is_ok());
    });

    state.wait_for_entered(1, Duration::from_secs(1))?;
    drop(release);

    let completed = result_rx.recv_timeout(Duration::from_secs(1))?;
    worker
        .join()
        .map_err(|_| std::io::Error::other("guard test worker panicked"))?;
    assert!(
        completed,
        "dropping the guard must release the blocked decoder"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn async_decode_count_wait_has_a_bounded_timeout() -> TestResult {
    let state = Arc::new(BlockingDecodeState::default());
    let wait_state = Arc::clone(&state);
    let wait = tokio::spawn(async move {
        wait_for_decode_count_with_timeout(&wait_state, 1, Duration::from_secs(1)).await
    });
    let bounded_wait =
        tokio::spawn(async move { tokio::time::timeout(Duration::from_secs(2), wait).await });

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(3)).await;

    let join_result = bounded_wait
        .await?
        .map_err(|_| std::io::Error::other("async decode wait ignored its outer timeout"))?;
    assert!(join_result?.is_err());
    Ok(())
}

fn run_infinite_ready_stream(
    chunk: Bytes,
    advance_deadline: bool,
) -> Result<usize, Box<dyn Error>> {
    let polls = Arc::new(AtomicUsize::new(0));
    let thread_polls = Arc::clone(&polls);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let url = Url::parse("https://example.invalid/infinite?token=DO_NOT_EXPOSE")?;
    let worker = thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
        else {
            let _ = result_tx.send(false);
            return;
        };
        let completed = runtime.block_on(async move {
            let fetcher = InfiniteReadyFetcher {
                chunk,
                polls: thread_polls,
            };
            let mut service = CachedArtworkService::new(fetcher, 1);
            if advance_deadline {
                let load = tokio::spawn(async move {
                    service
                        .load(&url, CellSize::new(1, 1), ColorCapability::TrueColor)
                        .await
                });
                tokio::task::yield_now().await;
                tokio::time::advance(ARTWORK_LOAD_TIMEOUT + Duration::from_millis(1)).await;
                matches!(load.await, Ok(ArtworkPresentation::Fallback(_)))
            } else {
                matches!(
                    service
                        .load(&url, CellSize::new(1, 1), ColorCapability::TrueColor)
                        .await,
                    ArtworkPresentation::Fallback(_)
                )
            }
        });
        let _ = result_tx.send(completed);
    });

    let completed = result_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| {
            std::io::Error::other("always-ready artwork stream monopolized the runtime")
        })?;
    worker
        .join()
        .map_err(|_| std::io::Error::other("always-ready stream worker panicked"))?;
    assert!(completed);
    Ok(polls.load(Ordering::SeqCst))
}

fn run_infinite_ready_cancellation() -> Result<(usize, usize), Box<dyn Error>> {
    let polls = Arc::new(AtomicUsize::new(0));
    let thread_polls = Arc::clone(&polls);
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let url = Url::parse("https://example.invalid/infinite?token=DO_NOT_EXPOSE")?;
    let worker = thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
        else {
            let _ = result_tx.send(None);
            return;
        };
        let completed = runtime.block_on(async move {
            let fetcher = InfiniteReadyFetcher {
                chunk: Bytes::new(),
                polls: Arc::clone(&thread_polls),
            };
            let mut service = CachedArtworkService::new(fetcher, 1);
            let load = tokio::spawn(async move {
                service
                    .load(&url, CellSize::new(1, 1), ColorCapability::TrueColor)
                    .await
            });
            let abort = load.abort_handle();
            let cancellation_polls = Arc::clone(&thread_polls);
            let cancellation = thread::spawn(move || {
                let wait_deadline = StdInstant::now() + Duration::from_secs(1);
                while cancellation_polls.load(Ordering::SeqCst) == 0
                    && StdInstant::now() < wait_deadline
                {
                    thread::yield_now();
                }
                let polls_at_abort = cancellation_polls.load(Ordering::SeqCst);
                abort.abort();
                polls_at_abort
            });
            let cancelled = matches!(load.await, Err(error) if error.is_cancelled());
            let polls_at_abort = cancellation.join().ok()?;
            Some((cancelled, polls_at_abort))
        });
        let _ = result_tx.send(completed);
    });

    let Some((cancelled, polls_at_abort)) = result_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| std::io::Error::other("always-ready artwork stream ignored cancellation"))?
    else {
        return Err(std::io::Error::other("cancellation worker failed").into());
    };
    worker
        .join()
        .map_err(|_| std::io::Error::other("cancellation worker panicked"))?;
    assert!(cancelled, "the hot fetch task must observe cancellation");
    Ok((polls_at_abort, polls.load(Ordering::SeqCst)))
}

fn one_image_fetcher(image: Vec<u8>) -> ChunkFetcher {
    ChunkFetcher {
        chunks: [Bytes::from(image)].into(),
        calls: Arc::new(AtomicUsize::new(0)),
        polls: Arc::new(AtomicUsize::new(0)),
    }
}

async fn wait_for_decode_count(
    state: &BlockingDecodeState,
    expected: usize,
) -> Result<(), std::io::Error> {
    wait_for_decode_count_with_timeout(state, expected, TEST_WAIT_TIMEOUT).await
}

async fn wait_for_decode_count_with_timeout(
    state: &BlockingDecodeState,
    expected: usize,
    timeout: Duration,
) -> Result<(), std::io::Error> {
    tokio::time::timeout(timeout, async {
        while state.entered() < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out waiting for {expected} decoder calls"),
        )
    })
}

fn encoded_png() -> Result<Vec<u8>, image::ImageError> {
    let Some(image) = RgbaImage::from_raw(1, 2, vec![10, 20, 30, 255, 40, 50, 60, 255]) else {
        unreachable!("the fixed pixel count matches the dimensions");
    };
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image).write_to(&mut bytes, ImageFormat::Png)?;
    Ok(bytes.into_inner())
}

#[derive(Default)]
struct BlockingDecodeState {
    counters: Mutex<DecodeCounters>,
    changed: Condvar,
}

#[derive(Default)]
struct DecodeCounters {
    entered: usize,
    active: usize,
    max_active: usize,
    released: usize,
}

impl BlockingDecodeState {
    fn enter_and_wait(&self, timeout: Duration) -> Result<(), ArtworkError> {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counters.entered += 1;
        let call = counters.entered;
        counters.active += 1;
        counters.max_active = counters.max_active.max(counters.active);
        self.changed.notify_all();
        let (mut counters, wait) = self
            .changed
            .wait_timeout_while(counters, timeout, |counters| counters.released < call)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if wait.timed_out() && counters.released < call {
            counters.active -= 1;
            self.changed.notify_all();
            return Err(ArtworkError::DecodeFailed);
        }
        counters.active -= 1;
        self.changed.notify_all();
        Ok(())
    }

    fn wait_for_entered(&self, expected: usize, timeout: Duration) -> Result<(), std::io::Error> {
        let counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (counters, wait) = self
            .changed
            .wait_timeout_while(counters, timeout, |counters| counters.entered < expected)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if wait.timed_out() && counters.entered < expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out waiting for {expected} decoder calls"),
            ));
        }
        Ok(())
    }

    fn release_through(&self, count: usize) {
        let mut counters = self
            .counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counters.released = counters.released.max(count);
        self.changed.notify_all();
    }

    fn entered(&self) -> usize {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entered
    }

    fn max_active(&self) -> usize {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .max_active
    }

    fn active(&self) -> usize {
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
    }
}

struct DecodeReleaseGuard {
    state: Arc<BlockingDecodeState>,
    through: usize,
}

impl DecodeReleaseGuard {
    fn all(state: Arc<BlockingDecodeState>) -> Self {
        Self::through(state, usize::MAX)
    }

    fn through(state: Arc<BlockingDecodeState>, through: usize) -> Self {
        Self { state, through }
    }
}

impl Drop for DecodeReleaseGuard {
    fn drop(&mut self) {
        self.state.release_through(self.through);
    }
}

struct BlockingDecoder {
    state: Arc<BlockingDecodeState>,
}

impl ArtworkDecoder for BlockingDecoder {
    fn decode(&self, bytes: Vec<u8>, size: CellSize) -> Result<ArtworkGrid, ArtworkError> {
        self.state.enter_and_wait(TEST_WAIT_TIMEOUT)?;
        decode_artwork(&bytes, size)
    }
}

struct ChunkFetcher {
    chunks: VecDeque<Bytes>,
    calls: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
}

#[async_trait]
impl ArtworkFetcher for ChunkFetcher {
    async fn fetch(&self, _url: &Url) -> Result<ArtworkByteStream, ArtworkFetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(CountingStream {
            chunks: self.chunks.clone(),
            polls: Arc::clone(&self.polls),
        }))
    }
}

struct CountingStream {
    chunks: VecDeque<Bytes>,
    polls: Arc<AtomicUsize>,
}

impl Stream for CountingStream {
    type Item = Result<Bytes, ArtworkFetchError>;

    fn poll_next(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(self.chunks.pop_front().map(Ok))
    }
}

struct InfiniteReadyFetcher {
    chunk: Bytes,
    polls: Arc<AtomicUsize>,
}

#[async_trait]
impl ArtworkFetcher for InfiniteReadyFetcher {
    async fn fetch(&self, _url: &Url) -> Result<ArtworkByteStream, ArtworkFetchError> {
        Ok(Box::pin(InfiniteReadyStream {
            chunk: self.chunk.clone(),
            polls: Arc::clone(&self.polls),
        }))
    }
}

struct InfiniteReadyStream {
    chunk: Bytes,
    polls: Arc<AtomicUsize>,
}

impl Stream for InfiniteReadyStream {
    type Item = Result<Bytes, ArtworkFetchError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Ready(Some(Ok(self.chunk.clone())))
    }
}

struct PendingFetcher {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ArtworkFetcher for PendingFetcher {
    async fn fetch(&self, _url: &Url) -> Result<ArtworkByteStream, ArtworkFetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::pending()))
    }
}
