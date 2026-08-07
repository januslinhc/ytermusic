use std::{
    collections::VecDeque,
    error::Error,
    future, io,
    panic::{self, PanicHookInfo},
    path::PathBuf,
    sync::mpsc as std_mpsc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use futures::stream;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use secrecy::SecretString;
use tokio::sync::{Notify, Semaphore, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use ytermusic::{
    app::{
        Action, AppErrorCategory, AppState, ArtworkSurface, DiagnosticCategory, Effect, Generation,
        SessionCheckpoint, reduce, stable_queue_item_id,
    },
    auth::{AuthenticatedProviderFactory, Browser},
    config::Config,
    domain::{
        ArtworkUrl, ChartSection, MediaId, MediaItem, MediaKind, PlaybackSnapshot, PlaybackStatus,
        RegionCode, RepeatMode, SearchFilter,
    },
    lyrics::{LyricsDocument, LyricsSource, TimedLyricLine},
    notifications::{NowPlayingNotification, RuntimeNotifier, RuntimeNotifierError},
    platform::{paths::AppPaths, signals::ShutdownSignals},
    podcast_rankings::{
        PodcastRankingError, PodcastRankingSource, PodcastRecommendationPage, parse_apple_top_shows,
    },
    provider::{
        AuthenticationState, BrowseItem, LibraryItem, LibrarySection, MusicProvider, Page,
        PlainLyrics, Podcast, ProviderError, ProviderErrorKind, ProviderOperation, ProviderResult,
        SearchItem,
    },
    queue::{QueueItem, QueueSnapshot},
    resolver::{PreviewStreamUrl, ResolvedStream},
    runtime::{
        ArtworkRuntimeComponents, EventSource, FifoStorage, MotionDemand, Renderer, Runtime,
        RuntimeAccount, RuntimeAccountService, RuntimeArtwork, RuntimeArtworkService, RuntimeClock,
        RuntimeCredentialImporter, RuntimeError, RuntimeEvent, RuntimeLyrics, RuntimeLyricsError,
        RuntimeMessage, RuntimePlayer, RuntimePlayerActions, RuntimePlayerError,
        RuntimeServiceError, RuntimeServices, RuntimeStorage, RuntimeStorageError,
        SharedMusicProvider, StartupFactory, TerminalControl, TerminalGuard, UiMotionTicker,
        bounded_action_channel, launch_application,
    },
    storage::{
        FavoriteEntry, FavoriteInsertOutcome, HistoryEntry, MetadataCacheEntry, PodcastProgress,
        Storage, StorageError,
    },
    ui::{
        animation::{
            AnimationDecoder, AnimationError, AnimationFrameOutput, AnimationFrameStore,
            AnimationKey, AnimationPacer, AnimationRequest, AnimationWorker,
        },
        artwork::{
            ArtworkByteStream, ArtworkFetchError, ArtworkFetcher, ArtworkGrid, ArtworkPresentation,
            ArtworkPresentationStore, CellSize, PRODUCTION_ARTWORK_SIZE, decode_rgb_frame,
        },
        interaction::{HitTarget, InteractionSnapshot, InteractionStore},
        render::{
            FocusRegion, NavigationItem, RenderModel, render_with_model as render_ui_with_model,
        },
        spectrum::{
            SpectrumDecoder, SpectrumError, SpectrumFrame, SpectrumFrameOutput, SpectrumFrameStore,
            SpectrumPacer, SpectrumRequest, SpectrumWorker,
        },
        theme::{ColorCapability, Theme},
    },
};

#[tokio::test(start_paused = true)]
async fn ui_motion_ticker_is_capped_and_coalesces_missed_ticks() -> TestResult {
    let mut ticker = UiMotionTicker::spawn();
    let mut redraw = ticker.redraw_receiver();
    ticker.set_demand(MotionDemand {
        progress: true,
        spinner: false,
        selection: false,
    });

    tokio::time::advance(Duration::from_millis(33)).await;
    tokio::task::yield_now().await;
    assert!(!redraw.has_changed()?);

    tokio::time::advance(Duration::from_millis(1)).await;
    redraw.changed().await?;
    assert_eq!(*redraw.borrow_and_update(), 1);

    tokio::time::advance(Duration::from_millis(340)).await;
    tokio::task::yield_now().await;
    redraw.changed().await?;
    assert_eq!(
        *redraw.borrow_and_update(),
        2,
        "missed deadlines must coalesce instead of replaying a backlog"
    );

    ticker.shutdown().await;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn ui_motion_ticker_obeys_each_bounded_demand_flag_and_shutdown() -> TestResult {
    for demand in [
        MotionDemand {
            progress: false,
            spinner: true,
            selection: false,
        },
        MotionDemand {
            progress: false,
            spinner: false,
            selection: true,
        },
    ] {
        let mut ticker = UiMotionTicker::spawn();
        let mut redraw = ticker.redraw_receiver();
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!redraw.has_changed()?);

        ticker.set_demand(demand);
        tokio::time::advance(Duration::from_millis(34)).await;
        redraw.changed().await?;
        assert_eq!(*redraw.borrow_and_update(), 1);

        ticker.set_demand(MotionDemand::default());
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(!redraw.has_changed()?);
        ticker.shutdown().await;
        assert!(redraw.changed().await.is_err());
    }
    Ok(())
}

struct UiMotionRenderer {
    snapshots: mpsc::UnboundedSender<(AppState, RenderModel)>,
    selection_active: Arc<std::sync::atomic::AtomicBool>,
}

impl Renderer for UiMotionRenderer {
    fn motion_demand(&self) -> MotionDemand {
        MotionDemand {
            selection: self.selection_active.load(Ordering::SeqCst),
            ..MotionDemand::default()
        }
    }

    fn render(&mut self, state: &AppState) -> io::Result<()> {
        self.render_with_model(state, &RenderModel::default())
    }

    fn render_with_model(&mut self, state: &AppState, model: &RenderModel) -> io::Result<()> {
        self.snapshots
            .send((state.clone(), model.clone()))
            .map_err(|_| io::Error::other("UI motion receiver closed"))
    }
}

#[tokio::test(start_paused = true)]
async fn ui_motion_runtime_renders_one_frame_per_tick_and_idles_when_paused() -> TestResult {
    let (events, event_rx) = mpsc::unbounded_channel();
    let (snapshots, mut rendered) = mpsc::unbounded_channel();
    let selection_active = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(AcceptingPlayer));
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        UiMotionRenderer {
            snapshots,
            selection_active: Arc::clone(&selection_active),
        },
    ));

    rendered.recv().await.ok_or("initial UI motion frame")?;
    let item = song("ui-motion-track");
    events.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: item.clone(),
    }))?;
    rendered.recv().await.ok_or("enqueue frame")?;
    events.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&item.id),
    }))?;
    let generation = rendered
        .recv()
        .await
        .and_then(|(state, _)| state.current_attempt_generation())
        .ok_or("playback generation")?;
    events.send(RuntimeEvent::Action(Action::PlayerProgress {
        generation,
        media_id: item.id.clone(),
        position_ms: 45_000,
        duration_ms: Some(180_000),
    }))?;
    rendered.recv().await.ok_or("progress frame")?;
    events.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Playing,
    }))?;
    let (_, playing) = rendered.recv().await.ok_or("playing frame")?;
    while rendered.try_recv().is_ok() {}

    tokio::time::advance(Duration::from_millis(34)).await;
    let (_, ticked) = rendered.recv().await.ok_or("motion tick frame")?;
    assert!(ticked.motion_frame().elapsed_ms >= playing.motion_frame().elapsed_ms + 34);
    assert!(ticked.motion_frame().progress.fraction > playing.motion_frame().progress.fraction);
    assert!(
        rendered.try_recv().is_err(),
        "one tick rendered more than once"
    );

    events.send(RuntimeEvent::Action(Action::PlayerProgress {
        generation,
        media_id: item.id.clone(),
        position_ms: 180_000,
        duration_ms: Some(180_000),
    }))?;
    let (_, ended) = rendered.recv().await.ok_or("end-of-track frame")?;
    assert!((ended.motion_frame().progress.fraction - 1.0).abs() < f64::EPSILON);
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(
        rendered.try_recv().is_err(),
        "exact end-of-track progress kept ticking"
    );

    events.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Paused,
    }))?;
    let (_, paused) = rendered.recv().await.ok_or("paused frame")?;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(rendered.try_recv().is_err(), "paused progress kept ticking");

    selection_active.store(true, Ordering::SeqCst);
    events.send(RuntimeEvent::Redraw)?;
    rendered.recv().await.ok_or("selection demand frame")?;
    tokio::time::advance(Duration::from_millis(34)).await;
    let (_, selection_tick) = rendered.recv().await.ok_or("selection tick")?;
    assert_eq!(
        selection_tick.motion_frame().progress,
        paused.motion_frame().progress
    );

    selection_active.store(false, Ordering::SeqCst);
    tokio::time::advance(Duration::from_millis(34)).await;
    rendered.recv().await.ok_or("selection completion frame")?;
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(
        rendered.try_recv().is_err(),
        "completed selection kept ticking"
    );

    events.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

struct RecordingNotifier {
    sent: mpsc::UnboundedSender<NowPlayingNotification>,
}

#[derive(Default)]
struct NonCooperativeRuntimeNotifier {
    started: Notify,
}

#[async_trait]
impl RuntimeNotifier for NonCooperativeRuntimeNotifier {
    async fn notify(
        &self,
        _notification: NowPlayingNotification,
        _cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        self.started.notify_one();
        future::pending().await
    }
}

#[async_trait]
impl RuntimeNotifier for RecordingNotifier {
    async fn notify(
        &self,
        notification: NowPlayingNotification,
        _cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        self.sent
            .send(notification)
            .map_err(|_| RuntimeNotifierError::unavailable())
    }
}

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn mouse_runtime_contract_is_typed_and_renderer_default_is_empty() {
    let mouse = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 7,
        modifiers: KeyModifiers::NONE,
    };
    assert_eq!(RuntimeEvent::Mouse(mouse), RuntimeEvent::Mouse(mouse));
    assert_eq!(RuntimeMessage::Mouse(mouse), RuntimeMessage::Mouse(mouse));

    let (states, _receiver) = mpsc::unbounded_channel();
    let renderer = StateChannelRenderer { states };
    assert!(renderer.interaction_snapshot().is_none());
}

struct MouseInteractionRenderer {
    models: mpsc::UnboundedSender<RenderModel>,
    interactions: InteractionStore,
    suppress_next_regions: bool,
    snapshot_reads: Arc<AtomicUsize>,
}

impl Renderer for MouseInteractionRenderer {
    fn interaction_snapshot(&self) -> Option<InteractionSnapshot> {
        self.snapshot_reads.fetch_add(1, Ordering::SeqCst);
        self.interactions.latest().cloned()
    }

    fn invalidate_interactions(&mut self) {
        self.interactions.invalidate();
        self.suppress_next_regions = true;
    }

    fn render(&mut self, state: &AppState) -> io::Result<()> {
        self.render_with_model(state, &RenderModel::default())
    }

    fn render_with_model(&mut self, _state: &AppState, model: &RenderModel) -> io::Result<()> {
        self.models
            .send(model.clone())
            .map_err(|_| io::Error::other("runtime model receiver closed"))?;
        let mut map = self
            .interactions
            .begin_frame()
            .ok_or_else(|| io::Error::other("interaction revision exhausted"))?;
        if !self.suppress_next_regions {
            assert!(map.push(
                Rect::new(4, 7, 1, 1),
                HitTarget::Navigation(NavigationItem::Search),
            ));
        }
        self.suppress_next_regions = false;
        assert!(self.interactions.publish(map));
        Ok(())
    }
}

#[tokio::test]
async fn runtime_mouse_uses_latest_interaction_snapshot_and_renders_controller_change() -> TestResult
{
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 7,
        modifiers: KeyModifiers::NONE,
    };
    let (models, mut rendered) = mpsc::unbounded_channel();
    let snapshot_reads = Arc::new(AtomicUsize::new(0));
    let (events, event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(
        Runtime::new(
            Config::default(),
            RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })),
        )
        .run(
            ChannelEvents { receiver: event_rx },
            MouseInteractionRenderer {
                models,
                interactions: InteractionStore::default(),
                suppress_next_regions: false,
                snapshot_reads: Arc::clone(&snapshot_reads),
            },
        ),
    );

    let first = rendered.recv().await.ok_or("initial model")?;
    events.send(RuntimeEvent::Mouse(click))?;
    let clicked = rendered.recv().await.ok_or("mouse-updated model")?;
    assert_eq!(first.view, NavigationItem::Home);
    assert_eq!(clicked.view, NavigationItem::Search);
    assert_eq!(clicked.focus, FocusRegion::Navigation);
    assert!(
        rendered.try_recv().is_err(),
        "one supported mouse event emitted more than one frame"
    );
    events.send(RuntimeEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..click
    }))?;
    events.send(RuntimeEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        ..click
    }))?;
    let after_wheel = rendered.recv().await.ok_or("wheel-updated model")?;
    assert_eq!(after_wheel.view, NavigationItem::Charts);
    assert_eq!(snapshot_reads.load(Ordering::SeqCst), 1);
    assert!(rendered.try_recv().is_err());
    events.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn runtime_resize_invalidates_old_geometry_before_following_mouse_input() -> TestResult {
    let click = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 7,
        modifiers: KeyModifiers::NONE,
    };
    let (models, mut rendered) = mpsc::unbounded_channel();
    let snapshot_reads = Arc::new(AtomicUsize::new(0));
    let (events, event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(
        Runtime::new(
            Config::default(),
            RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })),
        )
        .run(
            ChannelEvents { receiver: event_rx },
            MouseInteractionRenderer {
                models,
                interactions: InteractionStore::default(),
                suppress_next_regions: false,
                snapshot_reads,
            },
        ),
    );

    let _initial = rendered.recv().await.ok_or("initial model")?;
    events.send(RuntimeEvent::Resize(80, 24))?;
    let resized = rendered.recv().await.ok_or("resized model")?;
    events.send(RuntimeEvent::Mouse(click))?;
    let after_mouse = rendered.recv().await.ok_or("mouse model")?;
    assert_eq!(resized.view, NavigationItem::Home);
    assert_eq!(after_mouse.view, NavigationItem::Home);
    events.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn unsupported_mouse_events_skip_snapshot_resolution_and_rendering() -> TestResult {
    let (models, mut rendered) = mpsc::unbounded_channel();
    let snapshot_reads = Arc::new(AtomicUsize::new(0));
    let (events, event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(
        Runtime::new(
            Config::default(),
            RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })),
        )
        .run(
            ChannelEvents { receiver: event_rx },
            MouseInteractionRenderer {
                models,
                interactions: InteractionStore::default(),
                suppress_next_regions: false,
                snapshot_reads: Arc::clone(&snapshot_reads),
            },
        ),
    );
    let _initial = rendered.recv().await.ok_or("initial model")?;
    for kind in [
        MouseEventKind::Moved,
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Down(MouseButton::Right),
        MouseEventKind::Down(MouseButton::Middle),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::ScrollLeft,
        MouseEventKind::ScrollRight,
    ] {
        events.send(RuntimeEvent::Mouse(MouseEvent {
            kind,
            column: 4,
            row: 7,
            modifiers: KeyModifiers::NONE,
        }))?;
    }
    events.send(RuntimeEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 7,
        modifiers: KeyModifiers::NONE,
    }))?;
    let clicked = rendered.recv().await.ok_or("supported mouse render")?;
    assert_eq!(clicked.view, NavigationItem::Search);
    assert_eq!(snapshot_reads.load(Ordering::SeqCst), 1);
    assert!(rendered.try_recv().is_err());
    events.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

struct PendingSpectrumDecoder {
    started: mpsc::UnboundedSender<SpectrumRequest>,
    outputs: mpsc::UnboundedSender<watch::Sender<Option<SpectrumFrameOutput>>>,
    reaped: mpsc::UnboundedSender<()>,
    decode_count: AtomicUsize,
}

#[async_trait]
impl SpectrumDecoder for PendingSpectrumDecoder {
    async fn decode(
        &self,
        request: SpectrumRequest,
        output: watch::Sender<Option<SpectrumFrameOutput>>,
        cancel: CancellationToken,
    ) -> Result<(), SpectrumError> {
        let _ = self.outputs.send(output.clone());
        if self.decode_count.fetch_add(1, Ordering::SeqCst) == 0 {
            let frame = SpectrumFrame::new(
                vec![7; usize::from(request.key().target().bands())].into_boxed_slice(),
            )
            .ok_or(SpectrumError::ResourceLimit)?;
            output.send_replace(Some(Ok(Arc::new(frame))));
        }
        let _ = self.started.send(request);
        cancel.cancelled().await;
        let _ = self.reaped.send(());
        Ok(())
    }
}

struct YieldSpectrumPacer;

#[async_trait]
impl SpectrumPacer for YieldSpectrumPacer {
    async fn wait(&self, _duration: Duration) {
        tokio::task::yield_now().await;
    }
}

struct SpectrumHarness {
    area: Arc<Mutex<Rect>>,
    store: Arc<SpectrumFrameStore>,
    events: mpsc::UnboundedSender<RuntimeEvent>,
    states: mpsc::UnboundedReceiver<AppState>,
    started: mpsc::UnboundedReceiver<SpectrumRequest>,
    outputs: mpsc::UnboundedReceiver<watch::Sender<Option<SpectrumFrameOutput>>>,
    reaped: mpsc::UnboundedReceiver<()>,
    task: tokio::task::JoinHandle<Result<(), ytermusic::runtime::RuntimeError>>,
}

fn spectrum_runtime(area: Rect, enabled: bool) -> SpectrumHarness {
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let (outputs_tx, outputs_rx) = mpsc::unbounded_channel();
    let (reaped_tx, reaped_rx) = mpsc::unbounded_channel();
    let store = Arc::new(SpectrumFrameStore::new());
    let spectrum = SpectrumWorker::spawn(
        Arc::new(PendingSpectrumDecoder {
            started: started_tx,
            outputs: outputs_tx,
            reaped: reaped_tx,
            decode_count: AtomicUsize::new(0),
        }),
        Arc::new(YieldSpectrumPacer),
        Arc::clone(&store),
        15,
    );
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(AcceptingPlayer))
        .with_spectrum(spectrum);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = mpsc::unbounded_channel();
    let area = Arc::new(Mutex::new(area));
    let mut config = Config::default();
    config.visualizer.enabled = enabled;
    let task = tokio::spawn(Runtime::new(config, services).run(
        ChannelEvents { receiver: event_rx },
        AreaStateRenderer {
            area: Arc::clone(&area),
            states: state_tx,
        },
    ));
    SpectrumHarness {
        area,
        store,
        events: event_tx,
        states: state_rx,
        started: started_rx,
        outputs: outputs_rx,
        reaped: reaped_rx,
        task,
    }
}

struct FailingSpectrumDecoder {
    started: mpsc::UnboundedSender<SpectrumRequest>,
}

#[async_trait]
impl SpectrumDecoder for FailingSpectrumDecoder {
    async fn decode(
        &self,
        request: SpectrumRequest,
        output: watch::Sender<Option<SpectrumFrameOutput>>,
        _cancel: CancellationToken,
    ) -> Result<(), SpectrumError> {
        let _ = self.started.send(request);
        output.send_replace(Some(Err(SpectrumError::DecodeFailed)));
        Ok(())
    }
}

fn failing_spectrum_runtime(area: Rect) -> SpectrumHarness {
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let (_outputs_tx, outputs_rx) = mpsc::unbounded_channel();
    let (_reaped_tx, reaped_rx) = mpsc::unbounded_channel();
    let store = Arc::new(SpectrumFrameStore::new());
    let spectrum = SpectrumWorker::spawn(
        Arc::new(FailingSpectrumDecoder {
            started: started_tx,
        }),
        Arc::new(YieldSpectrumPacer),
        Arc::clone(&store),
        15,
    );
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(AcceptingPlayer))
        .with_spectrum(spectrum);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = mpsc::unbounded_channel();
    let area = Arc::new(Mutex::new(area));
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        AreaStateRenderer {
            area: Arc::clone(&area),
            states: state_tx,
        },
    ));
    SpectrumHarness {
        area,
        store,
        events: event_tx,
        states: state_rx,
        started: started_rx,
        outputs: outputs_rx,
        reaped: reaped_rx,
        task,
    }
}

async fn start_spectrum_playback(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    state_rx: &mut mpsc::UnboundedReceiver<AppState>,
) -> Result<(Generation, MediaId), Box<dyn Error>> {
    let item = song("spectrum-track");
    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: item.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&item.id),
    }))?;
    let generation = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(generation) = state_rx
                .recv()
                .await
                .and_then(|state| state.current_attempt_generation())
            {
                return generation;
            }
        }
    })
    .await?;
    event_tx.send(RuntimeEvent::Action(Action::AnalysisStreamUpdated {
        generation,
        stream_url: ResolvedStream::from_raw_audio_url(
            item.id.clone(),
            "https://audio.invalid/spectrum?secret=hidden",
            time::OffsetDateTime::UNIX_EPOCH,
        )?
        .analysis_stream_url(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Playing,
    }))?;
    Ok((generation, item.id))
}

async fn assert_spectrum_render_quiescent(
    states: &mut mpsc::UnboundedReceiver<AppState>,
) -> TestResult {
    while states.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        states.try_recv().is_err(),
        "ineligible spectrum state must not create a redraw feedback loop"
    );
    Ok(())
}

#[tokio::test]
async fn stopped_spectrum_runtime_remains_quiescent() -> TestResult {
    let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    tokio::time::timeout(Duration::from_secs(1), harness.states.recv())
        .await?
        .ok_or("initial render missing")?;
    assert_spectrum_render_quiescent(&mut harness.states).await?;
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_starts_in_wide_and_compact_but_not_tiny() -> TestResult {
    for (area, rows) in [(Rect::new(0, 0, 120, 32), 3), (Rect::new(0, 0, 80, 24), 1)] {
        let mut harness = spectrum_runtime(area, true);
        start_spectrum_playback(&harness.events, &mut harness.states).await?;
        let request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
            .await?
            .ok_or("spectrum did not start")?;
        assert_eq!(request.key().target().rows(), rows);
        assert!(request.key().target().bands() <= 64);
        harness.events.send(RuntimeEvent::Quit)?;
        harness.task.await??;
    }

    let mut harness = spectrum_runtime(Rect::new(0, 0, 40, 10), true);
    start_spectrum_playback(&harness.events, &mut harness.states).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), harness.started.recv())
            .await
            .is_err()
    );
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_disabled_or_missing_url_never_starts() -> TestResult {
    let mut disabled = spectrum_runtime(Rect::new(0, 0, 120, 32), false);
    start_spectrum_playback(&disabled.events, &mut disabled.states).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), disabled.started.recv())
            .await
            .is_err()
    );
    disabled.events.send(RuntimeEvent::Quit)?;
    disabled.task.await??;

    let mut missing = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    let item = song("missing-analysis-url");
    missing
        .events
        .send(RuntimeEvent::Action(Action::EnqueueMedia {
            item: item.clone(),
        }))?;
    missing
        .events
        .send(RuntimeEvent::Action(Action::PlayQueueItem {
            id: stable_queue_item_id(&item.id),
        }))?;
    let generation = loop {
        if let Some(generation) = missing
            .states
            .recv()
            .await
            .and_then(|state| state.current_attempt_generation())
        {
            break generation;
        }
    };
    missing
        .events
        .send(RuntimeEvent::Action(Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        }))?;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), missing.started.recv())
            .await
            .is_err()
    );
    missing.events.send(RuntimeEvent::Quit)?;
    missing.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_pause_reaps_and_resume_uses_authoritative_position() -> TestResult {
    let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    let (generation, media_id) =
        start_spectrum_playback(&harness.events, &mut harness.states).await?;
    let first = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("spectrum did not start")?;
    let frozen = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(frame) = harness.store.presentation(first.key()).frame().cloned() {
                return frame;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    harness
        .events
        .send(RuntimeEvent::Action(Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Paused,
        }))?;
    tokio::time::timeout(Duration::from_secs(1), harness.reaped.recv())
        .await?
        .ok_or("paused decoder not reaped")?;
    let paused = harness.store.presentation(first.key());
    assert!(paused.paused());
    assert!(
        paused
            .frame()
            .is_some_and(|frame| Arc::ptr_eq(frame, &frozen))
    );
    harness
        .events
        .send(RuntimeEvent::Action(Action::PlayerProgress {
            generation,
            media_id,
            position_ms: 42_375,
            duration_ms: Some(180_000),
        }))?;
    harness
        .events
        .send(RuntimeEvent::Action(Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        }))?;
    let resumed = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("spectrum did not resume")?;
    assert_eq!(first.key(), resumed.key());
    assert_eq!(resumed.start_ms(), 42_375);
    assert!(
        harness
            .store
            .presentation(resumed.key())
            .frame()
            .is_some_and(|frame| Arc::ptr_eq(frame, &frozen)),
        "resume must preserve the frozen frame until a replacement arrives"
    );
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_runtime_replacement_and_same_key_restart_reject_stale_publishers() -> TestResult {
    let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    let (generation, media_id) =
        start_spectrum_playback(&harness.events, &mut harness.states).await?;
    let first_request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("first spectrum run missing")?;
    let first_output = tokio::time::timeout(Duration::from_secs(1), harness.outputs.recv())
        .await?
        .ok_or("first publisher authority missing")?;

    *harness.area.lock().map_err(|_| "spectrum area poisoned")? = Rect::new(0, 0, 80, 24);
    harness.events.send(RuntimeEvent::Redraw)?;
    let replacement_request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("layout replacement run missing")?;
    let replacement_output = tokio::time::timeout(Duration::from_secs(1), harness.outputs.recv())
        .await?
        .ok_or("replacement publisher authority missing")?;
    assert_ne!(first_request.key(), replacement_request.key());

    harness
        .events
        .send(RuntimeEvent::Action(Action::PlayerProgress {
            generation,
            media_id,
            position_ms: 30_000,
            duration_ms: Some(180_000),
        }))?;
    let restarted_request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("same-key seek restart missing")?;
    let current_output = tokio::time::timeout(Duration::from_secs(1), harness.outputs.recv())
        .await?
        .ok_or("current publisher authority missing")?;
    assert_eq!(replacement_request.key(), restarted_request.key());

    let stale_first = Arc::new(
        SpectrumFrame::new(
            vec![3; usize::from(first_request.key().target().bands())].into_boxed_slice(),
        )
        .ok_or("valid first stale frame")?,
    );
    first_output.send_replace(Some(Ok(stale_first)));
    replacement_output.send_replace(Some(Err(SpectrumError::DecodeFailed)));
    tokio::task::yield_now().await;
    let before_current = harness.store.presentation(restarted_request.key());
    assert!(!before_current.failed());
    assert!(before_current.frame().is_none());

    let current = Arc::new(
        SpectrumFrame::new(
            vec![11; usize::from(restarted_request.key().target().bands())].into_boxed_slice(),
        )
        .ok_or("valid current frame")?,
    );
    current_output.send_replace(Some(Ok(Arc::clone(&current))));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if harness
                .store
                .presentation(restarted_request.key())
                .frame()
                .is_some_and(|frame| Arc::ptr_eq(frame, &current))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_same_key_stream_refresh_replaces_decoder_and_rejects_stale_output() -> TestResult
{
    let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    let (generation, media_id) =
        start_spectrum_playback(&harness.events, &mut harness.states).await?;
    let first_request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("first spectrum run missing")?;
    let stale_output = tokio::time::timeout(Duration::from_secs(1), harness.outputs.recv())
        .await?
        .ok_or("first publisher authority missing")?;
    let refreshed_url = ResolvedStream::from_raw_audio_url(
        media_id,
        "https://audio.invalid/refreshed?secret=new",
        time::OffsetDateTime::UNIX_EPOCH,
    )?
    .analysis_stream_url()
    .ok_or("refreshed analysis URL")?;
    harness
        .events
        .send(RuntimeEvent::Action(Action::AnalysisStreamUpdated {
            generation,
            stream_url: Some(refreshed_url.clone()),
        }))?;
    let refreshed_request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("refreshed spectrum run missing")?;
    let refreshed_output = tokio::time::timeout(Duration::from_secs(1), harness.outputs.recv())
        .await?
        .ok_or("refreshed publisher authority missing")?;
    assert_eq!(first_request.key(), refreshed_request.key());
    assert!(refreshed_request.matches_stream_url(&refreshed_url));
    tokio::time::timeout(Duration::from_secs(1), harness.reaped.recv())
        .await?
        .ok_or("stale URL decoder not reaped")?;

    stale_output.send_replace(Some(Err(SpectrumError::DecodeFailed)));
    tokio::task::yield_now().await;
    assert!(!harness.store.presentation(refreshed_request.key()).failed());
    let current = Arc::new(
        SpectrumFrame::new(
            vec![13; usize::from(refreshed_request.key().target().bands())].into_boxed_slice(),
        )
        .ok_or("valid refreshed frame")?,
    );
    refreshed_output.send_replace(Some(Ok(Arc::clone(&current))));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !harness
            .store
            .presentation(refreshed_request.key())
            .frame()
            .is_some_and(|frame| Arc::ptr_eq(frame, &current))
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_runtime_shutdown_clears_presentation_and_reaps_decoder() -> TestResult {
    let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    start_spectrum_playback(&harness.events, &mut harness.states).await?;
    let request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("spectrum did not start")?;
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    tokio::time::timeout(Duration::from_secs(1), harness.reaped.recv())
        .await?
        .ok_or("shutdown did not reap spectrum decoder")?;
    let presentation = harness.store.presentation(request.key());
    assert!(presentation.frame().is_none());
    assert!(!presentation.paused());
    assert!(!presentation.failed());
    Ok(())
}

#[tokio::test]
async fn spectrum_analyzer_error_preserves_playback_and_other_presentations() -> TestResult {
    let mut harness = failing_spectrum_runtime(Rect::new(0, 0, 120, 32));
    start_spectrum_playback(&harness.events, &mut harness.states).await?;
    let request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("spectrum did not start")?;
    let baseline = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let state = harness.states.recv().await.ok_or("state stream closed")?;
            if state.playback().status == PlaybackStatus::Playing {
                return Ok::<AppState, Box<dyn Error>>(state);
            }
        }
    })
    .await??;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if harness.store.presentation(request.key()).failed() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    harness.events.send(RuntimeEvent::Redraw)?;
    let after = tokio::time::timeout(Duration::from_secs(1), harness.states.recv())
        .await?
        .ok_or("error redraw missing")?;
    assert_eq!(after.playback(), baseline.playback());
    assert_eq!(after.artwork(), baseline.artwork());
    assert_eq!(after.lyrics(), baseline.lyrics());
    assert_eq!(after.player_presentation(), baseline.player_presentation());
    let spectrum = harness.store.presentation(request.key());
    assert!(spectrum.failed());
    assert!(spectrum.frame().is_none());
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_large_progress_discontinuity_restarts_but_normal_ticks_do_not() -> TestResult {
    let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    let (generation, media_id) =
        start_spectrum_playback(&harness.events, &mut harness.states).await?;
    let _ = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("spectrum did not start")?;
    for position_ms in [25, 50, 75, 100] {
        harness
            .events
            .send(RuntimeEvent::Action(Action::PlayerProgress {
                generation,
                media_id: media_id.clone(),
                position_ms,
                duration_ms: Some(180_000),
            }))?;
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), harness.started.recv())
            .await
            .is_err()
    );
    harness
        .events
        .send(RuntimeEvent::Action(Action::PlayerProgress {
            generation,
            media_id,
            position_ms: 30_000,
            duration_ms: Some(180_000),
        }))?;
    let restarted = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("seek did not restart spectrum")?;
    assert_eq!(restarted.start_ms(), 30_000);
    harness
        .events
        .send(RuntimeEvent::Action(Action::PlayerProgress {
            generation,
            media_id: restarted.key().media_id().clone(),
            position_ms: 500,
            duration_ms: Some(180_000),
        }))?;
    let rewound = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("backward seek did not restart spectrum")?;
    assert_eq!(rewound.start_ms(), 500);
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_stop_and_failure_clear_and_reap_analysis() -> TestResult {
    for status in [PlaybackStatus::Stopped, PlaybackStatus::Failed] {
        let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
        let (generation, _) = start_spectrum_playback(&harness.events, &mut harness.states).await?;
        let request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
            .await?
            .ok_or("spectrum did not start")?;
        harness
            .events
            .send(RuntimeEvent::Action(Action::PlayerStatusChanged {
                generation,
                status,
            }))?;
        tokio::time::timeout(Duration::from_secs(1), harness.reaped.recv())
            .await?
            .ok_or("terminal player status did not reap analyzer")?;
        assert!(harness.store.presentation(request.key()).frame().is_none());
        harness.events.send(RuntimeEvent::Quit)?;
        harness.task.await??;
    }
    Ok(())
}

#[tokio::test]
async fn spectrum_layout_downgrade_clears_and_reaps_active_analysis() -> TestResult {
    let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    start_spectrum_playback(&harness.events, &mut harness.states).await?;
    let request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("spectrum did not start")?;
    *harness.area.lock().map_err(|_| "spectrum area poisoned")? = Rect::new(0, 0, 40, 10);
    harness.events.send(RuntimeEvent::Redraw)?;
    tokio::time::timeout(Duration::from_secs(1), harness.reaped.recv())
        .await?
        .ok_or("tiny layout did not reap spectrum decoder")?;
    assert!(harness.store.presentation(request.key()).frame().is_none());
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

#[tokio::test]
async fn spectrum_frames_wake_runtime_render_and_bursts_coalesce() -> TestResult {
    let mut harness = spectrum_runtime(Rect::new(0, 0, 120, 32), true);
    start_spectrum_playback(&harness.events, &mut harness.states).await?;
    let request = tokio::time::timeout(Duration::from_secs(1), harness.started.recv())
        .await?
        .ok_or("spectrum did not start")?;
    let output = tokio::time::timeout(Duration::from_secs(1), harness.outputs.recv())
        .await?
        .ok_or("spectrum publisher authority missing")?;
    while harness.states.try_recv().is_ok() {}

    let frame = Arc::new(
        SpectrumFrame::new(vec![7; usize::from(request.key().target().bands())].into_boxed_slice())
            .ok_or("valid spectrum frame")?,
    );
    for _ in 0..256 {
        output.send_replace(Some(Ok(Arc::clone(&frame))));
    }

    tokio::time::timeout(Duration::from_secs(1), harness.states.recv())
        .await?
        .ok_or("spectrum publication did not wake renderer")?;
    tokio::time::sleep(Duration::from_millis(25)).await;
    let mut additional_renders = 0;
    while harness.states.try_recv().is_ok() {
        additional_renders += 1;
    }
    assert!(
        additional_renders < 256,
        "watch redraws must coalesce bursts"
    );
    harness.events.send(RuntimeEvent::Quit)?;
    harness.task.await??;
    Ok(())
}

struct PendingAnimationDecoder {
    started: mpsc::UnboundedSender<()>,
    reaped: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl AnimationDecoder for PendingAnimationDecoder {
    async fn decode(
        &self,
        _request: AnimationRequest,
        _output: watch::Sender<Option<AnimationFrameOutput>>,
        cancel: CancellationToken,
    ) -> Result<(), AnimationError> {
        let _ = self.started.send(());
        cancel.cancelled().await;
        let _ = self.reaped.send(());
        Ok(())
    }
}

struct YieldAnimationPacer;

#[async_trait]
impl AnimationPacer for YieldAnimationPacer {
    async fn wait(&self, _duration: Duration) {
        tokio::task::yield_now().await;
    }
}

struct AreaStateRenderer {
    area: Arc<Mutex<Rect>>,
    states: mpsc::UnboundedSender<AppState>,
}

impl Renderer for AreaStateRenderer {
    fn area(&self) -> Option<Rect> {
        self.area.lock().ok().map(|area| *area)
    }

    fn render(&mut self, state: &AppState) -> io::Result<()> {
        self.states
            .send(state.clone())
            .map_err(|_| io::Error::other("runtime state receiver closed"))
    }
}

struct AnimationHarness {
    area: Arc<Mutex<Rect>>,
    store: Arc<AnimationFrameStore>,
    events: mpsc::UnboundedSender<RuntimeEvent>,
    states: mpsc::UnboundedReceiver<AppState>,
    started: mpsc::UnboundedReceiver<()>,
    reaped: mpsc::UnboundedReceiver<()>,
    task: tokio::task::JoinHandle<Result<(), ytermusic::runtime::RuntimeError>>,
}

fn animation_runtime(area: Rect) -> AnimationHarness {
    let (started_tx, started_rx) = mpsc::unbounded_channel();
    let (reaped_tx, reaped_rx) = mpsc::unbounded_channel();
    let store = Arc::new(AnimationFrameStore::new());
    let animation = AnimationWorker::spawn(
        Arc::new(PendingAnimationDecoder {
            started: started_tx,
            reaped: reaped_tx,
        }),
        Arc::new(YieldAnimationPacer),
        Arc::clone(&store),
        8,
    );
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(AcceptingPlayer))
        .with_animation(animation);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = mpsc::unbounded_channel();
    let area = Arc::new(Mutex::new(area));
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        AreaStateRenderer {
            area: Arc::clone(&area),
            states: state_tx,
        },
    ));
    AnimationHarness {
        area,
        store,
        events: event_tx,
        states: state_rx,
        started: started_rx,
        reaped: reaped_rx,
        task,
    }
}

async fn start_video_playback(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    state_rx: &mut mpsc::UnboundedReceiver<AppState>,
) -> Result<(Generation, MediaId), Box<dyn Error>> {
    let mut item = song("animated-video");
    item.kind = MediaKind::Video;
    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: item.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&item.id),
    }))?;
    let generation = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(generation) = state_rx
                .recv()
                .await
                .and_then(|state| state.current_attempt_generation())
            {
                return generation;
            }
        }
    })
    .await?;
    let preview = PreviewStreamUrl::parse("https://video.invalid/animation-preview")?;
    event_tx.send(RuntimeEvent::Action(Action::PreviewStreamUpdated {
        generation,
        preview_url: Some(preview),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Playing,
    }))?;
    Ok((generation, item.id))
}

#[tokio::test]
async fn animation_starts_only_in_wide_layout_and_shutdown_reaps_decoder() -> TestResult {
    let AnimationHarness {
        area: _area,
        store: _store,
        events,
        mut states,
        mut started,
        mut reaped,
        task,
    } = animation_runtime(Rect::new(0, 0, 120, 32));
    start_video_playback(&events, &mut states).await?;
    tokio::time::timeout(Duration::from_secs(1), started.recv())
        .await?
        .ok_or("animation did not start")?;
    events.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), task).await???;
    tokio::time::timeout(Duration::from_secs(1), reaped.recv())
        .await?
        .ok_or("animation was not reaped")?;
    Ok(())
}

#[tokio::test]
async fn compact_and_tiny_layouts_never_start_animation_work() -> TestResult {
    for area in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 40, 10)] {
        let AnimationHarness {
            area: _area,
            store: _store,
            events,
            mut states,
            mut started,
            reaped: _reaped,
            task,
        } = animation_runtime(area);
        start_video_playback(&events, &mut states).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), started.recv())
                .await
                .is_err()
        );
        events.send(RuntimeEvent::Quit)?;
        tokio::time::timeout(Duration::from_secs(1), task).await???;
    }
    Ok(())
}

#[tokio::test]
async fn layout_downgrade_cancels_and_reaps_active_animation() -> TestResult {
    let AnimationHarness {
        area,
        store: _store,
        events,
        mut states,
        mut started,
        mut reaped,
        task,
    } = animation_runtime(Rect::new(0, 0, 120, 32));
    start_video_playback(&events, &mut states).await?;
    tokio::time::timeout(Duration::from_secs(1), started.recv())
        .await?
        .ok_or("animation did not start")?;
    *area.lock().map_err(|_| "animation area poisoned")? = Rect::new(0, 0, 80, 24);
    events.send(RuntimeEvent::Redraw)?;
    tokio::time::timeout(Duration::from_secs(1), reaped.recv())
        .await?
        .ok_or("layout downgrade did not reap animation")?;
    events.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), task).await???;
    Ok(())
}

#[tokio::test]
async fn paused_player_state_freezes_the_last_animation_frame() -> TestResult {
    let AnimationHarness {
        area: _area,
        store,
        events,
        mut states,
        mut started,
        mut reaped,
        task,
    } = animation_runtime(Rect::new(0, 0, 120, 32));
    let (generation, media_id) = start_video_playback(&events, &mut states).await?;
    tokio::time::timeout(Duration::from_secs(1), started.recv())
        .await?
        .ok_or("animation did not start")?;
    let key = AnimationKey::new(generation, media_id, PRODUCTION_ARTWORK_SIZE);
    let first = runtime_animation_frame([180, 20, 20])?;
    assert!(store.publish(&key, Arc::clone(&first)));

    events.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Paused,
    }))?;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let state = states.recv().await.ok_or("state stream closed")?;
            if state.playback().status == PlaybackStatus::Paused {
                return Ok::<(), Box<dyn Error>>(());
            }
        }
    })
    .await??;

    tokio::time::timeout(Duration::from_secs(1), reaped.recv())
        .await?
        .ok_or("paused animation decoder was not stopped and reaped")?;

    assert_eq!(store.presentation(&key).as_deref(), Some(first.as_ref()));
    assert!(!store.publish(&key, runtime_animation_frame([20, 20, 180])?));
    assert_eq!(store.presentation(&key).as_deref(), Some(first.as_ref()));

    events.send(RuntimeEvent::Action(Action::PlayerProgress {
        generation,
        media_id: key.media_id().clone(),
        position_ms: 42_375,
        duration_ms: Some(180_000),
    }))?;
    events.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Playing,
    }))?;
    tokio::time::timeout(Duration::from_secs(1), started.recv())
        .await?
        .ok_or("resumed animation decoder did not restart")?;

    events.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), task).await???;
    Ok(())
}

#[tokio::test]
async fn published_animation_frame_triggers_runtime_redraw_without_external_event() -> TestResult {
    let AnimationHarness {
        area: _area,
        store,
        events,
        mut states,
        mut started,
        reaped: _reaped,
        task,
    } = animation_runtime(Rect::new(0, 0, 120, 32));
    let (generation, media_id) = start_video_playback(&events, &mut states).await?;
    tokio::time::timeout(Duration::from_secs(1), started.recv())
        .await?
        .ok_or("animation did not start")?;
    events.send(RuntimeEvent::Action(Action::PlayerProgress {
        generation,
        media_id: media_id.clone(),
        position_ms: 12_345,
        duration_ms: Some(180_000),
    }))?;
    loop {
        let state = states.recv().await.ok_or("state stream closed")?;
        if state.playback().position_ms == 12_345 {
            break;
        }
    }
    while states.try_recv().is_ok() {}

    let key = AnimationKey::new(generation, media_id, PRODUCTION_ARTWORK_SIZE);
    assert!(store.publish(&key, runtime_animation_frame([20, 180, 20])?));

    tokio::time::timeout(Duration::from_secs(1), states.recv())
        .await?
        .ok_or("frame publication did not wake the runtime renderer")?;
    events.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), task).await???;
    Ok(())
}

fn runtime_animation_frame(rgb: [u8; 3]) -> Result<Arc<ArtworkGrid>, Box<dyn Error>> {
    let cells = usize::from(PRODUCTION_ARTWORK_SIZE.width)
        .saturating_mul(usize::from(PRODUCTION_ARTWORK_SIZE.height))
        .saturating_mul(2);
    let pixels = std::iter::repeat_n(rgb, cells)
        .flatten()
        .collect::<Vec<_>>();
    Ok(Arc::new(decode_rgb_frame(
        &pixels,
        PRODUCTION_ARTWORK_SIZE,
    )?))
}

struct LyricsCancellationGuard(mpsc::UnboundedSender<String>, String);

impl Drop for LyricsCancellationGuard {
    fn drop(&mut self) {
        let _ = self.0.send(self.1.clone());
    }
}

struct PendingLyrics {
    started: mpsc::UnboundedSender<String>,
    cancelled: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl RuntimeLyrics for PendingLyrics {
    async fn load(&self, item: &MediaItem) -> Result<Option<LyricsDocument>, RuntimeLyricsError> {
        let id = item.id.video_id.clone();
        self.started
            .send(id.clone())
            .map_err(|_| RuntimeLyricsError::unavailable())?;
        let _guard = LyricsCancellationGuard(self.cancelled.clone(), id);
        future::pending().await
    }
}

struct FailingLyrics;

#[async_trait]
impl RuntimeLyrics for FailingLyrics {
    async fn load(&self, _item: &MediaItem) -> Result<Option<LyricsDocument>, RuntimeLyricsError> {
        Err(RuntimeLyricsError::unavailable())
    }
}

#[tokio::test]
async fn replacing_playback_cancels_superseded_lyrics_work() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_lyrics(Arc::new(PendingLyrics {
            started: started_tx,
            cancelled: cancelled_tx,
        }))
        .with_player(Arc::new(AcceptingPlayer));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let first = song("lyrics-first");
    let second = song("lyrics-second");
    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: first.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: second.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&first.id),
    }))?;
    assert_eq!(started_rx.recv().await.as_deref(), Some("lyrics-first"));
    let first_generation = loop {
        let state = state_rx.recv().await.ok_or("state stream closed")?;
        if state.lyrics().media_id() == Some(&first.id) {
            break state
                .lyrics()
                .active_generation()
                .ok_or("first lyrics generation missing")?;
        }
    };
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&second.id),
    }))?;
    assert_eq!(cancelled_rx.recv().await.as_deref(), Some("lyrics-first"));
    assert_eq!(started_rx.recv().await.as_deref(), Some("lyrics-second"));
    let second_generation = loop {
        let state = state_rx.recv().await.ok_or("state stream closed")?;
        if state.lyrics().media_id() == Some(&second.id) {
            break state
                .lyrics()
                .active_generation()
                .ok_or("second lyrics generation missing")?;
        }
    };
    while state_rx.try_recv().is_ok() {}
    event_tx.send(RuntimeEvent::Action(Action::LyricsCompleted {
        generation: first_generation,
        media_id: first.id.into(),
        result: Ok(Some(LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![TimedLyricLine::new(0, None, "stale secret lyric")?],
            false,
        )?)),
    }))?;
    let state = state_rx.recv().await.ok_or("state stream closed")?;
    assert_eq!(state.lyrics().media_id(), Some(&second.id));
    assert_eq!(state.lyrics().active_generation(), Some(second_generation));
    assert!(state.lyrics().document().is_none());
    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn shutdown_cancels_pending_lyrics_and_source_failure_is_playback_safe() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_lyrics(
        Arc::new(PendingLyrics {
            started: started_tx,
            cancelled: cancelled_tx,
        }),
    );
    let item = song("lyrics-shutdown");
    let services = services
        .with_initial_action(Action::EnqueueMedia { item: item.clone() })
        .with_initial_action(Action::PlayQueueItem {
            id: stable_queue_item_id(&item.id),
        });
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    assert_eq!(started_rx.recv().await.as_deref(), Some("lyrics-shutdown"));
    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    assert_eq!(
        cancelled_rx.recv().await.as_deref(),
        Some("lyrics-shutdown")
    );

    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_lyrics(Arc::new(FailingLyrics))
        .with_player(Arc::new(AcceptingPlayer));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let failed = song("lyrics-failure");
    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: failed.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&failed.id),
    }))?;
    let mut observed = false;
    for _ in 0..12 {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("state stream closed")?;
        if state.lyrics().error().is_some() {
            assert_ne!(state.playback().status, PlaybackStatus::Failed);
            observed = true;
            break;
        }
    }
    assert!(observed);
    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[test]
fn lyrics_action_debug_output_redacts_content() -> TestResult {
    let provider_sentinel = "sentinel-lyrics-provider";
    let video_sentinel = "sentinel-lyrics-video-id";
    let title_sentinel = "sentinel-lyrics-title";
    let mut item = song(video_sentinel);
    item.id.provider = provider_sentinel.to_owned();
    item.title = title_sentinel.to_owned();
    let document = LyricsDocument::new(
        LyricsSource::Lrclib,
        None,
        vec![TimedLyricLine::new(0, None, "secret lyric content")?],
        false,
    )?;
    let requested = Action::LyricsRequested {
        item: item.clone().into(),
    };
    let effect = Effect::LoadLyrics {
        generation: Generation::new(1),
        item: item.clone().into(),
    };
    let completed = Action::LyricsCompleted {
        generation: Generation::new(1),
        media_id: item.id.clone().into(),
        result: Ok(Some(document)),
    };
    let queue_id = stable_queue_item_id(&item.id);
    let (state, _) = reduce(AppState::default(), Action::EnqueueMedia { item });
    let (state, _) = reduce(state, Action::PlayQueueItem { id: queue_id });
    let debug = format!(
        "{requested:?} {effect:?} {completed:?} {:?}",
        state.lyrics()
    );
    for sentinel in [
        "secret lyric content",
        provider_sentinel,
        video_sentinel,
        title_sentinel,
    ] {
        assert!(!debug.contains(sentinel), "lyrics debug leaked {sentinel}");
    }
    Ok(())
}

#[test]
fn platform_paths_keep_config_data_cache_and_logs_separated() {
    let paths = AppPaths::from_roots(
        PathBuf::from("/config-root"),
        PathBuf::from("/data-root"),
        PathBuf::from("/cache-root"),
    );

    assert_eq!(
        paths.config_file(),
        PathBuf::from("/config-root/config.toml")
    );
    assert_eq!(
        paths.database_file(),
        PathBuf::from("/data-root/ytermusic.db")
    );
    assert_eq!(paths.log_directory(), PathBuf::from("/data-root/logs"));
    assert_eq!(
        paths.log_file(),
        PathBuf::from("/data-root/logs/ytermusic.log")
    );
    assert_eq!(paths.cache_directory(), PathBuf::from("/cache-root"));
}

#[tokio::test]
async fn injected_platform_signal_is_observed_once() -> TestResult {
    let (signal_tx, signal_rx) = mpsc::channel(1);
    let mut signals = ShutdownSignals::injected(signal_rx);

    signal_tx.send(()).await?;
    signals.recv().await?;
    drop(signal_tx);
    assert!(signals.recv().await.is_err());
    Ok(())
}

#[tokio::test]
async fn startup_pipeline_runs_in_secret_safe_pre_terminal_order() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    launch_application(&RecordingStartup {
        calls: Arc::clone(&calls),
    })
    .await?;

    assert_eq!(
        calls
            .lock()
            .map_err(|_| "startup calls poisoned")?
            .as_slice(),
        [
            "paths",
            "logging",
            "config",
            "storage",
            "credentials",
            "provider",
            "dependencies",
            "tui",
        ]
    );
    Ok(())
}

#[test]
fn startup_restores_queue_and_podcast_playback() -> TestResult {
    let episode = podcast_episode("restored-episode");
    let queue_id = stable_queue_item_id(&episode.id);
    let checkpoint = SessionCheckpoint {
        queue: QueueSnapshot {
            logical: vec![QueueItem::new(queue_id.clone(), episode.clone())],
            active: vec![queue_id.clone()],
            current: Some(queue_id),
            repeat: RepeatMode::All,
            shuffle_seed: Some(42),
            radio: true,
        },
        playback: PlaybackSnapshot {
            current: Some(episode.id.clone()),
            status: PlaybackStatus::Paused,
            position_ms: 91_000,
            duration_ms: episode.duration_ms,
            target_volume: 37,
            playback_speed: 1.5,
        },
    };

    let restored = AppState::restore_session(Config::default(), checkpoint)?;

    assert_eq!(
        restored.queue().current().map(QueueItem::media),
        Some(&episode)
    );
    assert_eq!(restored.queue().repeat(), RepeatMode::All);
    assert!(restored.queue().is_shuffled());
    assert!(restored.queue().radio_enabled());
    assert_eq!(restored.playback().current.as_ref(), Some(&episode.id));
    assert_eq!(restored.playback().position_ms, 91_000);
    assert_eq!(restored.playback().target_volume, 37);
    assert!((restored.playback().playback_speed - 1.5).abs() < f64::EPSILON);

    let (_, effects) = reduce(restored, Action::TogglePlayback);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadPodcastProgress { media_id, .. } if media_id == &episode.id
    )));
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Resolve { .. } | Effect::Player(_)))
    );
    Ok(())
}

#[test]
fn restored_transient_playback_status_is_normalized() -> TestResult {
    let episode = podcast_episode("transient-episode");
    let queue_id = stable_queue_item_id(&episode.id);

    for status in [
        PlaybackStatus::Resolving,
        PlaybackStatus::Buffering,
        PlaybackStatus::Playing,
    ] {
        let restored = AppState::restore_session(
            Config::default(),
            SessionCheckpoint {
                queue: QueueSnapshot {
                    logical: vec![QueueItem::new(queue_id.clone(), episode.clone())],
                    active: vec![queue_id.clone()],
                    current: Some(queue_id.clone()),
                    repeat: RepeatMode::Off,
                    shuffle_seed: None,
                    radio: false,
                },
                playback: PlaybackSnapshot {
                    current: Some(episode.id.clone()),
                    status,
                    position_ms: 12_000,
                    duration_ms: episode.duration_ms,
                    target_volume: 61,
                    playback_speed: 1.25,
                },
            },
        )?;

        assert_eq!(restored.playback().status, PlaybackStatus::Stopped);
        assert!(restored.current_attempt_generation().is_none());
        assert!(restored.current_resolve_generation().is_none());
        assert!(restored.player_presentation().fade().is_none());
        assert!(!restored.player_presentation().quality().known());
        assert!((restored.player_presentation().effective_volume() - 61.0).abs() < f64::EPSILON);
    }
    Ok(())
}

#[tokio::test]
async fn first_renderer_sees_restored_state() -> TestResult {
    let episode = podcast_episode("first-render-episode");
    let expected = episode.clone();
    let checkpoint = checkpoint_for(episode, PlaybackStatus::Playing);
    let rendered = Arc::new(Mutex::new(Vec::new()));
    let services = RuntimeServices::new(Arc::new(StartupStorage {
        checkpoint: Some(checkpoint),
    }));
    let runtime = Runtime::new(Config::default(), services);

    runtime
        .run(
            OneEvent::new(RuntimeEvent::Quit),
            RecordingRenderer {
                states: Arc::clone(&rendered),
            },
        )
        .await?;

    let states = rendered.lock().map_err(|_| "renderer state poisoned")?;
    let first = states
        .first()
        .ok_or("runtime did not render initial state")?;
    assert_eq!(
        first.queue().current().map(QueueItem::media),
        Some(&expected)
    );
    assert_eq!(first.playback().current.as_ref(), Some(&expected.id));
    assert_eq!(first.playback().status, PlaybackStatus::Stopped);
    Ok(())
}

#[tokio::test]
async fn bounded_action_channel_applies_backpressure_without_losing_actions() -> TestResult {
    let (sender, mut receiver) = bounded_action_channel(1);
    let first = RuntimeMessage::Action(Action::TargetVolumeChanged(10));
    let second = RuntimeMessage::Action(Action::TargetVolumeChanged(20));

    sender.send(first.clone()).await?;
    let mut blocked_send = Box::pin(sender.send(second.clone()));
    tokio::select! {
        biased;
        result = &mut blocked_send => {
            return Err(format!("second send bypassed backpressure: {result:?}").into());
        }
        () = tokio::task::yield_now() => {}
    }

    assert_eq!(receiver.recv().await, Some(first));
    blocked_send.await?;
    assert_eq!(receiver.recv().await, Some(second));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn effects_execute_concurrently_through_one_bounded_action_channel() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let provider = Arc::new(ConcurrentProvider {
        started: started_tx,
        release: Arc::clone(&release),
    });
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_provider(provider);
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::SearchSubmitted {
        query: "concurrent".to_owned(),
        filter: SearchFilter::Songs,
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::ChartsRequested {
        region: RegionCode::parse("HK")?,
    }))?;

    let first = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
        .await?
        .ok_or("provider start stream closed")?;
    let second = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
        .await?
        .ok_or("provider start stream closed")?;
    assert_ne!(first, second);
    assert!([first, second].contains(&"search"));
    assert!([first, second].contains(&"charts"));

    release.add_permits(2);
    let mut completed_together = false;
    for _ in 0..8 {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("runtime state stream closed")?;
        if !state.search().loading()
            && !state.search().items().is_empty()
            && !state.charts().loading()
            && !state.charts().sections().is_empty()
        {
            completed_together = true;
            break;
        }
    }
    assert!(completed_together);

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn bounded_internal_work_backpressures_a_slow_player_lane() -> TestResult {
    let release = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(BlockingVolumePlayer {
            started: started_tx,
            release: release.clone(),
        }))
        .with_action_capacity(1);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        AcknowledgedEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let (first_ack_tx, first_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Action(Action::TargetVolumeChanged(1)),
        first_ack_tx,
    ))?;
    first_ack_rx.await?;
    started_rx
        .recv()
        .await
        .ok_or("first player command did not start")?;

    let mut last_ack = None;
    for volume in 2..=64 {
        let (ack_tx, ack_rx) = oneshot::channel();
        event_tx.send((
            RuntimeEvent::Action(Action::TargetVolumeChanged(volume)),
            ack_tx,
        ))?;
        last_ack = Some(ack_rx);
    }
    let last_ack = last_ack.ok_or("player burst was empty")?;
    assert!(
        tokio::time::timeout(Duration::from_millis(1), last_ack)
            .await
            .is_err(),
        "slow player work was consumed into an unbounded internal backlog"
    );

    release.add_permits(64);
    let (quit_ack_tx, quit_ack_rx) = oneshot::channel();
    event_tx.send((RuntimeEvent::Quit, quit_ack_tx))?;
    quit_ack_rx.await?;
    runtime_task.await??;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn saturated_player_lane_cannot_deadlock_the_action_bus() -> TestResult {
    let release_failure = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(SaturatingFailPlayer {
            calls: AtomicUsize::new(0),
            started: started_tx,
            release_failure: release_failure.clone(),
        }))
        .with_action_capacity(1);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        AcknowledgedEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let (first_ack_tx, first_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Action(Action::TargetVolumeChanged(1)),
        first_ack_tx,
    ))?;
    first_ack_rx.await?;
    started_rx
        .recv()
        .await
        .ok_or("head player command did not start")?;

    let mut last_ack = None;
    for volume in 2..=11 {
        let (ack_tx, ack_rx) = oneshot::channel();
        event_tx.send((
            RuntimeEvent::Action(Action::TargetVolumeChanged(volume)),
            ack_tx,
        ))?;
        last_ack = Some(ack_rx);
    }
    let last_ack = last_ack.ok_or("saturation burst was empty")?;
    let _ = tokio::time::timeout(Duration::from_millis(1), last_ack).await;

    release_failure.add_permits(1);
    tokio::time::timeout(Duration::from_millis(1), started_rx.recv())
        .await
        .map_err(|_| "player lane and action bus formed a circular wait")?
        .ok_or("player start stream closed")?;

    let (quit_ack_tx, quit_ack_rx) = oneshot::channel();
    event_tx.send((RuntimeEvent::Quit, quit_ack_tx))?;
    quit_ack_rx.await?;
    runtime_task.await??;
    Ok(())
}

#[tokio::test]
async fn quit_cancels_a_dispatch_blocked_on_the_saturated_player_lane() -> TestResult {
    let release = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(BlockingVolumePlayer {
            started: started_tx,
            release,
        }))
        .with_action_capacity(16)
        .with_shutdown_timeout(Duration::from_millis(50));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    for volume in 1..=10 {
        event_tx.send(RuntimeEvent::Action(Action::TargetVolumeChanged(volume)))?;
    }
    started_rx
        .recv()
        .await
        .ok_or("head player command did not start")?;
    loop {
        let state = state_rx
            .recv()
            .await
            .ok_or("runtime state stream closed before saturation")?;
        if state.playback().target_volume == 10 {
            break;
        }
    }

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task)
        .await
        .map_err(|_| "terminal request did not cancel the blocked dispatcher")???;
    Ok(())
}

async fn assert_terminal_breaks_saturated_player_and_action_lanes(
    terminal: RuntimeEvent,
    label: &str,
) -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(CleanupStorage {
        calls: Arc::clone(&calls),
    }))
    .with_player(Arc::new(SaturatedCleanupPlayer {
        calls: Arc::clone(&calls),
        started: started_tx,
        release,
    }))
    .with_terminal(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))
    .with_action_capacity(1)
    .with_shutdown_timeout(Duration::from_millis(25));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        AcknowledgedEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    state_rx
        .recv()
        .await
        .ok_or("runtime did not render its initial state")?;

    let (first_ack_tx, first_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE)),
        first_ack_tx,
    ))?;
    first_ack_rx.await?;
    started_rx
        .recv()
        .await
        .ok_or("head player command did not start")?;

    for _ in 2..=10 {
        let (ack_tx, _ack_rx) = oneshot::channel();
        event_tx.send((
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE)),
            ack_tx,
        ))?;
    }
    for _ in 0..10 {
        state_rx
            .recv()
            .await
            .ok_or("runtime state stream closed before player-lane saturation")?;
    }

    let (queued_ack_tx, queued_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE)),
        queued_ack_tx,
    ))?;
    queued_ack_rx.await?;
    let (blocked_ack_tx, blocked_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE)),
        blocked_ack_tx,
    ))?;
    blocked_ack_rx.await?;
    let (terminal_ack_tx, _terminal_ack_rx) = oneshot::channel();
    event_tx.send((terminal, terminal_ack_tx))?;

    tokio::time::timeout(Duration::from_millis(26), runtime_task)
        .await
        .map_err(|_| format!("{label} was not observed after the action bus saturated"))???;

    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
    let shutdown = calls
        .iter()
        .position(|call| *call == "player_shutdown")
        .ok_or("player shutdown was not attempted")?;
    let abort = calls
        .iter()
        .position(|call| *call == "player_abort")
        .ok_or("blocked player was not aborted")?;
    let restore = calls
        .iter()
        .position(|call| *call == "disable_raw")
        .ok_or("terminal was not restored")?;
    assert!(shutdown < abort);
    assert!(abort < restore, "cleanup order changed: {calls:?}");
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn keyboard_quit_is_out_of_band_when_player_and_action_lanes_are_saturated() -> TestResult {
    assert_terminal_breaks_saturated_player_and_action_lanes(
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        "keyboard quit",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_is_out_of_band_when_player_and_action_lanes_are_saturated() -> TestResult {
    assert_terminal_breaks_saturated_player_and_action_lanes(RuntimeEvent::Signal, "signal").await
}

async fn assert_terminal_cancels_saturated_podcast_fallback(
    terminal: RuntimeEvent,
    label: &str,
) -> TestResult {
    let region = RegionCode::parse("US")?;
    let page = parse_apple_top_shows(
        br#"{"feed":{"country":"US","results":[{"id":"daily","name":"The Daily","artistName":"NYT"}]}}"#,
    )?;
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_action_capacity(1)
        .with_shutdown_timeout(Duration::from_millis(50))
        .with_initial_action(Action::PodcastRecommendationsRequested {
            region: region.clone(),
        })
        .with_initial_action(Action::PodcastRecommendationsCompleted {
            generation: Generation::new(1),
            requested_region: region,
            result: Ok(page),
        })
        .with_initial_action(Action::OpenSelectedPodcastRecommendation);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    event_tx.send(terminal)?;
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    tokio::time::timeout(Duration::from_secs(1), runtime_task)
        .await
        .map_err(|_| format!("{label} did not cancel the saturated podcast fallback"))???;
    Ok(())
}

#[tokio::test]
async fn keyboard_quit_cancels_a_saturated_podcast_fallback() -> TestResult {
    assert_terminal_cancels_saturated_podcast_fallback(RuntimeEvent::Quit, "keyboard quit").await
}

#[tokio::test]
async fn signal_cancels_a_saturated_podcast_fallback() -> TestResult {
    assert_terminal_cancels_saturated_podcast_fallback(RuntimeEvent::Signal, "signal").await
}

struct RecordingPodcastRankings {
    calls: AtomicUsize,
    page: PodcastRecommendationPage,
}

struct ReplacingPodcastRankings {
    started: mpsc::UnboundedSender<String>,
    cancelled: mpsc::UnboundedSender<()>,
    us_page: PodcastRecommendationPage,
}

struct PendingPodcastRankings {
    started: mpsc::UnboundedSender<()>,
    cancelled: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl PodcastRankingSource for PendingPodcastRankings {
    async fn top_shows(
        &self,
        _requested: &RegionCode,
    ) -> Result<PodcastRecommendationPage, PodcastRankingError> {
        self.started
            .send(())
            .map_err(|_| PodcastRankingError::Unavailable)?;
        let _notice = DropNotice {
            cancelled: self.cancelled.clone(),
        };
        future::pending::<()>().await;
        unreachable!("pending ranking request only exits through cancellation");
    }
}

#[async_trait]
impl PodcastRankingSource for ReplacingPodcastRankings {
    async fn top_shows(
        &self,
        requested: &RegionCode,
    ) -> Result<PodcastRecommendationPage, PodcastRankingError> {
        self.started
            .send(requested.as_str().to_owned())
            .map_err(|_| PodcastRankingError::Unavailable)?;
        if requested.as_str() == "JP" {
            let _notice = DropNotice {
                cancelled: self.cancelled.clone(),
            };
            future::pending::<()>().await;
            unreachable!("JP ranking request only exits through replacement");
        }
        Ok(self.us_page.clone())
    }
}

struct RecordingPodcastMatchProvider {
    calls: mpsc::UnboundedSender<(String, SearchFilter)>,
    podcast_calls: mpsc::UnboundedSender<String>,
    title: String,
    publisher: String,
    ambiguous: bool,
}

struct PendingPodcastMatchProvider {
    started: mpsc::UnboundedSender<()>,
    cancelled: mpsc::UnboundedSender<()>,
}

struct EmptyQueryRejectingProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl MusicProvider for EmptyQueryRejectingProvider {
    async fn search(
        &self,
        _query: &str,
        _filter: SearchFilter,
    ) -> ProviderResult<Page<SearchItem>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(unavailable(ProviderOperation::Search))
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        Err(unavailable(ProviderOperation::Charts))
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Playlist))
    }

    async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
        Err(unavailable(ProviderOperation::Podcast))
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Radio))
    }

    async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        Err(unavailable(ProviderOperation::Library))
    }

    fn authentication(&self) -> AuthenticationState {
        AuthenticationState::Unauthenticated
    }
}

#[async_trait]
impl MusicProvider for PendingPodcastMatchProvider {
    async fn search(&self, _query: &str, filter: SearchFilter) -> ProviderResult<Page<SearchItem>> {
        assert_eq!(filter, SearchFilter::Podcasts);
        self.started
            .send(())
            .map_err(|_| unavailable(ProviderOperation::Search))?;
        let _notice = DropNotice {
            cancelled: self.cancelled.clone(),
        };
        future::pending::<()>().await;
        unreachable!("pending match request only exits through cancellation");
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        Err(unavailable(ProviderOperation::Charts))
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Playlist))
    }

    async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
        Err(unavailable(ProviderOperation::Podcast))
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Radio))
    }

    async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        Err(unavailable(ProviderOperation::Library))
    }

    fn authentication(&self) -> AuthenticationState {
        AuthenticationState::Unauthenticated
    }
}

#[async_trait]
impl MusicProvider for RecordingPodcastMatchProvider {
    async fn search(&self, query: &str, filter: SearchFilter) -> ProviderResult<Page<SearchItem>> {
        self.calls
            .send((query.to_owned(), filter))
            .map_err(|_| unavailable(ProviderOperation::Search))?;
        let mut items = vec![SearchItem::Podcast(BrowseItem {
            id: "opaque-provider-id".to_owned(),
            title: self.title.clone(),
            subtitle: Some(self.publisher.clone()),
            artwork_url: None,
        })];
        if self.ambiguous {
            items.push(SearchItem::Podcast(BrowseItem {
                id: "second-provider-id".to_owned(),
                title: self.title.clone(),
                subtitle: Some(self.publisher.clone()),
                artwork_url: None,
            }));
        }
        Ok(Page {
            items,
            continuation: None,
            stale: false,
        })
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        Err(unavailable(ProviderOperation::Charts))
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Playlist))
    }

    async fn podcast(&self, id: &str) -> ProviderResult<Podcast> {
        self.podcast_calls
            .send(id.to_owned())
            .map_err(|_| unavailable(ProviderOperation::Podcast))?;
        Err(unavailable(ProviderOperation::Podcast))
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Radio))
    }

    async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        Err(unavailable(ProviderOperation::Library))
    }

    fn authentication(&self) -> AuthenticationState {
        AuthenticationState::Unauthenticated
    }
}

#[async_trait]
impl PodcastRankingSource for RecordingPodcastRankings {
    async fn top_shows(
        &self,
        requested: &RegionCode,
    ) -> Result<PodcastRecommendationPage, PodcastRankingError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(requested, self.page.region());
        Ok(self.page.clone())
    }
}

#[tokio::test]
async fn podcast_recommendation_source_runs_once_and_preserves_request_scope() -> TestResult {
    let page = parse_apple_top_shows(
        br#"{"feed":{"country":"JP","results":[{"id":"source","name":"Show","artistName":"Publisher"}]}}"#,
    )?;
    let source = Arc::new(RecordingPodcastRankings {
        calls: AtomicUsize::new(0),
        page: page.clone(),
    });
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_podcast_rankings(source.clone());
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(
        Action::PodcastRecommendationsRequested {
            region: RegionCode::parse("JP")?,
        },
    ))?;

    let completed = loop {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("runtime state stream closed")?;
        if !state.podcasts().recommendations_loading()
            && !state.podcasts().recommendations().is_empty()
        {
            break state;
        }
    };
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    assert_eq!(completed.podcasts().requested_region().as_str(), "JP");
    assert_eq!(completed.podcasts().effective_region(), Some(page.region()));
    assert_eq!(completed.podcasts().recommendation_generation().value(), 1);

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[tokio::test]
async fn podcast_recommendation_region_replacement_cancels_stale_request() -> TestResult {
    let us_page = parse_apple_top_shows(
        br#"{"feed":{"country":"US","results":[{"id":"source","name":"US Show","artistName":"Publisher"}]}}"#,
    )?;
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let source = Arc::new(ReplacingPodcastRankings {
        started: started_tx,
        cancelled: cancelled_tx,
        us_page,
    });
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_podcast_rankings(source);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(
        Action::PodcastRecommendationsRequested {
            region: RegionCode::parse("JP")?,
        },
    ))?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), started_rx.recv()).await?,
        Some("JP".to_owned())
    );
    event_tx.send(RuntimeEvent::Action(
        Action::PodcastRecommendationsRequested {
            region: RegionCode::parse("US")?,
        },
    ))?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), cancelled_rx.recv()).await?,
        Some(())
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), started_rx.recv()).await?,
        Some("US".to_owned())
    );

    let completed = loop {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("runtime state stream closed")?;
        if !state.podcasts().recommendations_loading()
            && state.podcasts().requested_region().as_str() == "US"
            && !state.podcasts().recommendations().is_empty()
        {
            break state;
        }
    };
    assert_eq!(
        completed
            .podcasts()
            .effective_region()
            .map(RegionCode::as_str),
        Some("US")
    );
    assert_eq!(completed.podcasts().recommendation_generation().value(), 2);

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[tokio::test]
async fn podcast_recommendation_match_uses_bounded_podcast_search() -> TestResult {
    let title = "t".repeat(512);
    let publisher = "p".repeat(512);
    let fixture = serde_json::to_vec(&serde_json::json!({
        "feed": {"country": "US", "results": [{
            "id": "source",
            "name": title,
            "artistName": publisher
        }]}
    }))?;
    let page = parse_apple_top_shows(&fixture)?;
    let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
    let (podcast_calls_tx, mut podcast_calls_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(RecordingPodcastMatchProvider {
        calls: calls_tx,
        podcast_calls: podcast_calls_tx,
        title,
        publisher,
        ambiguous: false,
    });
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_provider(provider)
        .with_initial_action(Action::PodcastRecommendationsRequested {
            region: RegionCode::parse("US")?,
        })
        .with_initial_action(Action::PodcastRecommendationsCompleted {
            generation: Generation::new(1),
            requested_region: RegionCode::parse("US")?,
            result: Ok(page),
        })
        .with_initial_action(Action::OpenSelectedPodcastRecommendation);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let (query, filter) = tokio::time::timeout(Duration::from_secs(1), calls_rx.recv())
        .await?
        .ok_or("podcast search call stream closed")?;
    assert_eq!(filter, SearchFilter::Podcasts);
    assert!(query.len() <= 512);
    assert!(query.is_char_boundary(query.len()));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), podcast_calls_rx.recv()).await?,
        Some("opaque-provider-id".to_owned())
    );

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[tokio::test]
async fn ambiguous_podcast_recommendation_match_publishes_safe_error() -> TestResult {
    let page = parse_apple_top_shows(
        br#"{"feed":{"country":"US","results":[{"id":"source","name":"Sensitive title","artistName":"Sensitive publisher"}]}}"#,
    )?;
    let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
    let (podcast_calls_tx, mut podcast_calls_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(RecordingPodcastMatchProvider {
        calls: calls_tx,
        podcast_calls: podcast_calls_tx,
        title: "Sensitive title".to_owned(),
        publisher: "Sensitive publisher".to_owned(),
        ambiguous: true,
    });
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_provider(provider)
        .with_initial_action(Action::PodcastRecommendationsRequested {
            region: RegionCode::parse("US")?,
        })
        .with_initial_action(Action::PodcastRecommendationsCompleted {
            generation: Generation::new(1),
            requested_region: RegionCode::parse("US")?,
            result: Ok(page),
        })
        .with_initial_action(Action::OpenSelectedPodcastRecommendation);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let _ = tokio::time::timeout(Duration::from_secs(1), calls_rx.recv())
        .await?
        .ok_or("podcast search call stream closed")?;
    let error = loop {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("runtime state stream closed")?;
        if let Some(error) = state.podcasts().resolve_error() {
            break error.clone();
        }
    };
    assert_eq!(error.category(), AppErrorCategory::Podcast);
    for sensitive in [
        "Sensitive title",
        "Sensitive publisher",
        "opaque-provider-id",
        "second-provider-id",
    ] {
        assert!(!error.message().contains(sensitive));
        assert!(!format!("{error:?}").contains(sensitive));
    }
    assert!(podcast_calls_rx.try_recv().is_err());

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[tokio::test]
async fn shutdown_cancels_pending_podcast_recommendation_boundaries() -> TestResult {
    let page = parse_apple_top_shows(
        br#"{"feed":{"country":"US","results":[{"id":"source","name":"Show","artistName":"Publisher"}]}}"#,
    )?;
    let (ranking_started_tx, mut ranking_started_rx) = mpsc::unbounded_channel();
    let (ranking_cancelled_tx, mut ranking_cancelled_rx) = mpsc::unbounded_channel();
    let (match_started_tx, mut match_started_rx) = mpsc::unbounded_channel();
    let (match_cancelled_tx, mut match_cancelled_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_podcast_rankings(Arc::new(PendingPodcastRankings {
            started: ranking_started_tx,
            cancelled: ranking_cancelled_tx,
        }))
        .with_provider(Arc::new(PendingPodcastMatchProvider {
            started: match_started_tx,
            cancelled: match_cancelled_tx,
        }))
        .with_initial_action(Action::PodcastRecommendationsRequested {
            region: RegionCode::parse("US")?,
        })
        .with_initial_action(Action::PodcastRecommendationsCompleted {
            generation: Generation::new(1),
            requested_region: RegionCode::parse("US")?,
            result: Ok(page),
        })
        .with_initial_action(Action::OpenSelectedPodcastRecommendation);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    tokio::time::timeout(Duration::from_secs(1), ranking_started_rx.recv())
        .await?
        .ok_or("ranking boundary did not start")?;
    tokio::time::timeout(Duration::from_secs(1), match_started_rx.recv())
        .await?
        .ok_or("match boundary did not start")?;
    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), ranking_cancelled_rx.recv()).await?,
        Some(())
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), match_cancelled_rx.recv()).await?,
        Some(())
    );
    Ok(())
}

#[tokio::test]
async fn empty_bounded_podcast_match_query_never_reaches_provider() -> TestResult {
    let oversized_grapheme = format!("a{}", "\u{301}".repeat(130));
    let fixture = serde_json::to_vec(&serde_json::json!({
        "feed": {"country": "US", "results": [{
            "id": "source",
            "name": oversized_grapheme,
            "artistName": oversized_grapheme
        }]}
    }))?;
    let page = parse_apple_top_shows(&fixture)?;
    let calls = Arc::new(AtomicUsize::new(0));
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_provider(Arc::new(EmptyQueryRejectingProvider {
            calls: Arc::clone(&calls),
        }))
        .with_initial_action(Action::PodcastRecommendationsRequested {
            region: RegionCode::parse("US")?,
        })
        .with_initial_action(Action::PodcastRecommendationsCompleted {
            generation: Generation::new(1),
            requested_region: RegionCode::parse("US")?,
            result: Ok(page),
        })
        .with_initial_action(Action::OpenSelectedPodcastRecommendation);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let error = loop {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("runtime state stream closed")?;
        if let Some(error) = state.podcasts().resolve_error() {
            break error.clone();
        }
    };
    assert_eq!(error.category(), AppErrorCategory::Podcast);
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

async fn assert_control_breaks_producer_owned_action_bus(
    events: Vec<RuntimeEvent>,
    label: &str,
) -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (action_accepted_tx, action_accepted_rx) = oneshot::channel();
    let mut services = RuntimeServices::new(Arc::new(CleanupStorage {
        calls: Arc::clone(&calls),
    }))
    .with_player(Arc::new(SaturatedCleanupPlayer {
        calls: Arc::clone(&calls),
        started: started_tx,
        release: release.clone(),
    }))
    .with_player_actions(Box::new(ActionAfterEventTask {
        event_finished: None,
        actions: VecDeque::from([Action::TargetVolumeChanged(99)]),
        action_accepted: Some(action_accepted_tx),
    }))
    .with_terminal(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))
    .with_action_capacity(1)
    .with_shutdown_timeout(Duration::from_millis(25));
    for volume in 1..=10 {
        services = services.with_initial_action(Action::TargetVolumeChanged(volume));
    }

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let mut runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    started_rx
        .recv()
        .await
        .ok_or("head player command did not start")?;
    action_accepted_rx
        .await
        .map_err(|_| "producer action was not accepted before the terminal key")?;

    for event in events {
        event_tx.send(event)?;
    }
    let observed = tokio::time::timeout(Duration::from_millis(26), &mut runtime_task).await;
    if observed.is_err() {
        release.add_permits(64);
        if tokio::time::timeout(Duration::from_millis(100), &mut runtime_task)
            .await
            .is_err()
        {
            runtime_task.abort();
            let _ = runtime_task.await;
        }
    }
    observed.map_err(|_| {
        format!("{label} was not observed while a producer owned the saturated action bus")
    })???;

    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
    let shutdown = calls
        .iter()
        .position(|call| *call == "player_shutdown")
        .ok_or("player shutdown was not attempted")?;
    let abort = calls
        .iter()
        .position(|call| *call == "player_abort")
        .ok_or("blocked player was not aborted")?;
    let restore = calls
        .iter()
        .position(|call| *call == "disable_raw")
        .ok_or("terminal was not restored")?;
    assert!(shutdown < abort);
    assert!(abort < restore, "cleanup order changed: {calls:?}");
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn lone_keyboard_quit_is_out_of_band_when_producer_owns_saturated_action_bus() -> TestResult {
    assert_control_breaks_producer_owned_action_bus(
        vec![RuntimeEvent::Key(KeyEvent::new(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
        ))],
        "lone keyboard quit",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn lone_ctrl_c_is_out_of_band_when_producer_owns_saturated_action_bus() -> TestResult {
    assert_control_breaks_producer_owned_action_bus(
        vec![RuntimeEvent::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ))],
        "lone Ctrl-C",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_bypasses_two_pending_ordinary_events_under_saturation() -> TestResult {
    assert_control_breaks_producer_owned_action_bus(
        vec![
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            RuntimeEvent::Signal,
        ],
        "signal behind two pending ordinary events",
    )
    .await
}

fn pending_key_events(depth: usize, mode_changing_first: bool) -> Vec<RuntimeEvent> {
    (0..depth)
        .map(|index| {
            let character = if mode_changing_first && index == 0 {
                '/'
            } else {
                'x'
            };
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        })
        .collect()
}

async fn assert_deep_control_bypass(
    depth: usize,
    mode_changing_first: bool,
    terminal: RuntimeEvent,
    label: &str,
) -> TestResult {
    let mut events = pending_key_events(depth, mode_changing_first);
    events.push(terminal);
    assert_control_breaks_producer_owned_action_bus(events, label).await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_bypasses_three_pending_ordinary_events_under_saturation() -> TestResult {
    assert_deep_control_bypass(
        3,
        true,
        RuntimeEvent::Signal,
        "signal behind three pending ordinary events",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_bypasses_more_than_pending_capacity_under_saturation() -> TestResult {
    assert_deep_control_bypass(
        64,
        true,
        RuntimeEvent::Signal,
        "signal behind more than pending capacity",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn ctrl_c_bypasses_two_pending_ordinary_events_under_saturation() -> TestResult {
    assert_deep_control_bypass(
        2,
        true,
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        "Ctrl-C behind two pending ordinary events",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn ctrl_c_bypasses_three_pending_ordinary_events_under_saturation() -> TestResult {
    assert_deep_control_bypass(
        3,
        true,
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        "Ctrl-C behind three pending ordinary events",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn ctrl_c_bypasses_more_than_pending_capacity_under_saturation() -> TestResult {
    assert_deep_control_bypass(
        64,
        true,
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        "Ctrl-C behind more than pending capacity",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn keyboard_quit_bypasses_two_safe_pending_events_under_saturation() -> TestResult {
    assert_deep_control_bypass(
        2,
        false,
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        "keyboard quit behind two safe pending events",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn keyboard_quit_bypasses_three_safe_pending_events_under_saturation() -> TestResult {
    assert_deep_control_bypass(
        3,
        false,
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        "keyboard quit behind three safe pending events",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn keyboard_quit_bypasses_more_than_pending_capacity_under_saturation() -> TestResult {
    assert_deep_control_bypass(
        64,
        false,
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        "keyboard quit behind more than pending capacity",
    )
    .await
}

fn mapped_normal_key_events(depth: usize) -> Vec<RuntimeEvent> {
    (0..depth)
        .map(|_| RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE)))
        .collect()
}

async fn assert_control_bypasses_mapped_key_overload(
    terminal: RuntimeEvent,
    label: &str,
) -> TestResult {
    let mut events = mapped_normal_key_events(64);
    events.push(terminal);
    assert_control_breaks_producer_owned_action_bus(events, label).await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_bypasses_more_than_pending_capacity_of_mapped_keys() -> TestResult {
    assert_control_bypasses_mapped_key_overload(
        RuntimeEvent::Signal,
        "signal behind mapped-key overload",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn ctrl_c_bypasses_more_than_pending_capacity_of_mapped_keys() -> TestResult {
    assert_control_bypasses_mapped_key_overload(
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        "Ctrl-C behind mapped-key overload",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn keyboard_quit_bypasses_more_than_pending_capacity_of_mapped_keys() -> TestResult {
    assert_control_bypasses_mapped_key_overload(
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        "keyboard quit behind mapped-key overload",
    )
    .await
}

async fn assert_control_bypasses_text_entry_overload(
    opener: char,
    terminal: RuntimeEvent,
    label: &str,
) -> TestResult {
    let mut events = vec![RuntimeEvent::Key(KeyEvent::new(
        KeyCode::Char(opener),
        KeyModifiers::NONE,
    ))];
    events.extend(
        (0..64).map(|_| RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))),
    );
    events.push(terminal);
    assert_control_breaks_producer_owned_action_bus(events, label).await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_bypasses_search_text_entry_overload() -> TestResult {
    assert_control_bypasses_text_entry_overload(
        '/',
        RuntimeEvent::Signal,
        "signal behind search text-entry overload",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn ctrl_c_bypasses_search_text_entry_overload() -> TestResult {
    assert_control_bypasses_text_entry_overload(
        '/',
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        "Ctrl-C behind search text-entry overload",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_bypasses_palette_text_entry_overload() -> TestResult {
    assert_control_bypasses_text_entry_overload(
        ':',
        RuntimeEvent::Signal,
        "signal behind palette text-entry overload",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn ctrl_c_bypasses_palette_text_entry_overload() -> TestResult {
    assert_control_bypasses_text_entry_overload(
        ':',
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        "Ctrl-C behind palette text-entry overload",
    )
    .await
}

async fn assert_closing_key_projects_normal_under_saturation(
    opener: char,
    closing: KeyCode,
    label: &str,
) -> TestResult {
    assert_control_breaks_producer_owned_action_bus(
        vec![
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char(opener), KeyModifiers::NONE)),
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            RuntimeEvent::Key(KeyEvent::new(closing, KeyModifiers::NONE)),
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        ],
        label,
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn search_enter_projects_normal_before_queued_q_under_saturation() -> TestResult {
    assert_closing_key_projects_normal_under_saturation(
        '/',
        KeyCode::Enter,
        "q behind projected search Enter",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn search_escape_projects_normal_before_queued_q_under_saturation() -> TestResult {
    assert_closing_key_projects_normal_under_saturation(
        '/',
        KeyCode::Esc,
        "q behind projected search Escape",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn palette_escape_projects_normal_before_queued_q_under_saturation() -> TestResult {
    assert_closing_key_projects_normal_under_saturation(
        ':',
        KeyCode::Esc,
        "q behind projected palette Escape",
    )
    .await
}

#[derive(Clone, Copy)]
enum PaletteOutcome {
    SearchText,
    NormalQuit,
}

fn palette_submit_events(query: &str, trailing: RuntimeEvent) -> Vec<RuntimeEvent> {
    let mut events = vec![RuntimeEvent::Key(KeyEvent::new(
        KeyCode::Char(':'),
        KeyModifiers::NONE,
    ))];
    events.extend(query.chars().map(|character| {
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
    }));
    events.push(RuntimeEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    events.push(trailing);
    events
}

async fn send_acknowledged_event(
    event_tx: &mpsc::UnboundedSender<(RuntimeEvent, oneshot::Sender<()>)>,
    event: RuntimeEvent,
) -> TestResult {
    let (ack_tx, ack_rx) = oneshot::channel();
    event_tx.send((event, ack_tx))?;
    ack_rx.await?;
    Ok(())
}

async fn assert_palette_ack(query: &str, expectation: PaletteOutcome) -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (action_accepted_tx, action_accepted_rx) = oneshot::channel();
    let mut services = RuntimeServices::new(Arc::new(CleanupStorage {
        calls: Arc::clone(&calls),
    }))
    .with_player(Arc::new(SaturatedCleanupPlayer {
        calls: Arc::clone(&calls),
        started: started_tx,
        release: release.clone(),
    }))
    .with_player_actions(Box::new(ActionAfterEventTask {
        event_finished: None,
        actions: VecDeque::from([Action::TargetVolumeChanged(99)]),
        action_accepted: Some(action_accepted_tx),
    }))
    .with_terminal(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))
    .with_action_capacity(2)
    .with_shutdown_timeout(Duration::from_millis(25));
    for volume in 1..=10 {
        services = services.with_initial_action(Action::TargetVolumeChanged(volume));
    }

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel();
    let mut runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        AcknowledgedEvents { receiver: event_rx },
        ModelChannelRenderer {
            snapshots: snapshot_tx,
        },
    ));
    started_rx
        .recv()
        .await
        .ok_or("head player command did not start")?;
    action_accepted_rx
        .await
        .map_err(|_| "producer action was not accepted before palette input")?;

    for event in palette_submit_events(
        query,
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
    ) {
        send_acknowledged_event(&event_tx, event).await?;
    }

    if let Ok(result) = tokio::time::timeout(Duration::from_millis(26), &mut runtime_task).await {
        result??;
        return Err(
            format!("palette query {query:?} let q quit before Enter reached the reducer").into(),
        );
    }

    release.add_permits(64);
    match expectation {
        PaletteOutcome::SearchText => {
            tokio::time::timeout(Duration::from_millis(1), async {
                loop {
                    let (state, model) = snapshot_rx
                        .recv()
                        .await
                        .ok_or("runtime snapshot stream closed before search q was rendered")?;
                    let mut terminal = Terminal::new(TestBackend::new(90, 30))?;
                    terminal.draw(|frame| {
                        render_ui_with_model(frame, &state, &Theme::default(), &model);
                    })?;
                    if terminal
                        .backend()
                        .to_string()
                        .contains("Query: q  ·  Filter:")
                    {
                        return Ok::<(), Box<dyn Error>>(());
                    }
                }
            })
            .await
            .map_err(|_| "palette search did not retain the admitted q as search text")??;

            for key in [
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            ] {
                send_acknowledged_event(&event_tx, RuntimeEvent::Key(key)).await?;
            }
            tokio::time::timeout(Duration::from_millis(26), runtime_task)
                .await
                .map_err(|_| "palette search cleanup did not enable q after submit")???;
        }
        PaletteOutcome::NormalQuit => {
            tokio::time::timeout(Duration::from_millis(26), runtime_task)
                .await
                .map_err(
                    |_| "normal palette command did not process the admitted q after ack",
                )???;
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn palette_search_submit_keeps_queued_q_as_search_text_under_saturation() -> TestResult {
    assert_palette_ack("search", PaletteOutcome::SearchText).await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn palette_normal_submit_defers_queued_q_until_reducer_ack_under_saturation() -> TestResult {
    assert_palette_ack("shuffle", PaletteOutcome::NormalQuit).await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_bypasses_ambiguous_palette_enter_under_saturation() -> TestResult {
    assert_control_breaks_producer_owned_action_bus(
        palette_submit_events("search", RuntimeEvent::Signal),
        "signal behind ambiguous palette Enter",
    )
    .await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn ctrl_c_bypasses_ambiguous_palette_enter_under_saturation() -> TestResult {
    assert_control_breaks_producer_owned_action_bus(
        palette_submit_events(
            "search",
            RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        ),
        "Ctrl-C behind ambiguous palette Enter",
    )
    .await
}

async fn assert_mode_opener_precedes_queued_q(
    opener: char,
    transition: KeyCode,
    expected_render: &str,
    label: &str,
) -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let release = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (action_accepted_tx, action_accepted_rx) = oneshot::channel();
    let mut services = RuntimeServices::new(Arc::new(CleanupStorage {
        calls: Arc::clone(&calls),
    }))
    .with_player(Arc::new(SaturatedCleanupPlayer {
        calls: Arc::clone(&calls),
        started: started_tx,
        release: release.clone(),
    }))
    .with_player_actions(Box::new(ActionAfterEventTask {
        event_finished: None,
        actions: VecDeque::from([Action::TargetVolumeChanged(99)]),
        action_accepted: Some(action_accepted_tx),
    }))
    .with_terminal(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))
    .with_action_capacity(2)
    .with_shutdown_timeout(Duration::from_millis(25));
    for volume in 1..=10 {
        services = services.with_initial_action(Action::TargetVolumeChanged(volume));
    }

    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (snapshot_tx, mut snapshot_rx) = mpsc::unbounded_channel();
    let mut runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        AcknowledgedEvents { receiver: event_rx },
        ModelChannelRenderer {
            snapshots: snapshot_tx,
        },
    ));
    started_rx
        .recv()
        .await
        .ok_or("head player command did not start")?;
    action_accepted_rx
        .await
        .map_err(|_| "producer action was not accepted before the mode opener")?;

    for key in [
        KeyEvent::new(KeyCode::Char(opener), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    ] {
        let (ack_tx, ack_rx) = oneshot::channel();
        event_tx.send((RuntimeEvent::Key(key), ack_tx))?;
        ack_rx.await?;
    }

    if let Ok(result) = tokio::time::timeout(Duration::from_millis(26), &mut runtime_task).await {
        result??;
        return Err(
            format!("prequeued {label} then q quit before the opener reached the reducer").into(),
        );
    }

    release.add_permits(64);
    tokio::time::timeout(Duration::from_millis(1), async {
        loop {
            let (state, model) = snapshot_rx
                .recv()
                .await
                .ok_or("runtime snapshot stream closed before q was rendered")?;
            let mut terminal = Terminal::new(TestBackend::new(90, 30))?;
            terminal.draw(|frame| {
                render_ui_with_model(frame, &state, &Theme::default(), &model);
            })?;
            if terminal.backend().to_string().contains(expected_render) {
                return Ok::<(), Box<dyn Error>>(());
            }
        }
    })
    .await
    .map_err(|_| format!("{label} did not retain q as text after the player lane resumed"))??;

    for key in [
        KeyEvent::new(transition, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
    ] {
        let (ack_tx, ack_rx) = oneshot::channel();
        event_tx.send((RuntimeEvent::Key(key), ack_tx))?;
        ack_rx.await?;
    }
    tokio::time::timeout(Duration::from_millis(26), runtime_task)
        .await
        .map_err(|_| format!("{label} transition back to normal mode did not enable q"))???;
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn search_opener_reaches_the_reducer_before_queued_q_can_quit() -> TestResult {
    assert_mode_opener_precedes_queued_q('/', KeyCode::Enter, "Query: xq  ·  Filter:", "'/'").await
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn palette_opener_reaches_the_reducer_before_queued_q_can_quit() -> TestResult {
    assert_mode_opener_precedes_queued_q(':', KeyCode::Esc, "Query: xq", "':'").await
}

#[tokio::test(start_paused = true)]
async fn bounded_internal_work_backpressures_a_slow_storage_lane() -> TestResult {
    let release = Arc::new(Semaphore::new(0));
    let (save_started_tx, mut save_started_rx) = mpsc::unbounded_channel();
    let storage = Arc::new(BlockingPodcastStorage {
        save_started: save_started_tx,
        release: release.clone(),
    });
    let services = RuntimeServices::new(storage)
        .with_player(Arc::new(AcceptingPlayer))
        .with_action_capacity(1);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        AcknowledgedEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let episode = podcast_episode("bounded-storage-episode");

    for action in [
        Action::EnqueueMedia {
            item: episode.clone(),
        },
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    ] {
        let (ack_tx, ack_rx) = oneshot::channel();
        event_tx.send((RuntimeEvent::Action(action), ack_tx))?;
        ack_rx.await?;
    }
    let generation = loop {
        let state = state_rx.recv().await.ok_or("runtime state stream closed")?;
        if let Some(generation) = state.current_attempt_generation() {
            break generation;
        }
    };

    let (first_ack_tx, first_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Action(Action::PlayerProgress {
            generation,
            media_id: episode.id.clone(),
            position_ms: 1,
            duration_ms: episode.duration_ms,
        }),
        first_ack_tx,
    ))?;
    first_ack_rx.await?;
    save_started_rx
        .recv()
        .await
        .ok_or("first podcast save did not start")?;

    let mut last_ack = None;
    for position_ms in 2..=64 {
        let (ack_tx, ack_rx) = oneshot::channel();
        event_tx.send((
            RuntimeEvent::Action(Action::PlayerProgress {
                generation,
                media_id: episode.id.clone(),
                position_ms,
                duration_ms: episode.duration_ms,
            }),
            ack_tx,
        ))?;
        last_ack = Some(ack_rx);
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            last_ack.ok_or("storage burst was empty")?,
        )
        .await
        .is_err(),
        "slow storage work was consumed into an unbounded internal backlog"
    );

    release.add_permits(64);
    let (quit_ack_tx, quit_ack_rx) = oneshot::channel();
    event_tx.send((RuntimeEvent::Quit, quit_ack_tx))?;
    quit_ack_rx.await?;
    runtime_task.await??;
    Ok(())
}

async fn assert_accepted_progress_survives_cancelled_durable_admission(
    terminal: RuntimeEvent,
    label: &str,
) -> TestResult {
    let episode = podcast_episode(&format!("{label}-durable-progress"));
    let (save_started_tx, mut save_started_rx) = mpsc::unbounded_channel();
    let (saved_tx, mut saved_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let services = RuntimeServices::new(Arc::new(SaturatingPodcastStorage {
        save_started: save_started_tx,
        saved: saved_tx,
        release: release.clone(),
    }))
    .with_player(Arc::new(AcceptingPlayer))
    .with_action_capacity(4)
    .with_shutdown_timeout(Duration::from_millis(100));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: episode.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&episode.id),
    }))?;
    let generation = loop {
        let state = state_rx.recv().await.ok_or("runtime state stream closed")?;
        if let Some(generation) = state.current_attempt_generation() {
            break generation;
        }
    };

    event_tx.send(RuntimeEvent::Action(Action::PlayerProgress {
        generation,
        media_id: episode.id.clone(),
        position_ms: 1,
        duration_ms: episode.duration_ms,
    }))?;
    loop {
        let progress = save_started_rx
            .recv()
            .await
            .ok_or("podcast save stream closed")?;
        if progress.position_ms == 1 {
            break;
        }
        release.add_permits(1);
    }

    for position_ms in 2..=18 {
        event_tx.send(RuntimeEvent::Action(Action::PlayerProgress {
            generation,
            media_id: episode.id.clone(),
            position_ms,
            duration_ms: episode.duration_ms,
        }))?;
    }
    loop {
        let state = state_rx.recv().await.ok_or("runtime state stream closed")?;
        if state.playback().position_ms == 18 {
            break;
        }
    }

    event_tx.send(terminal)?;
    release.add_permits(32);
    tokio::time::timeout(Duration::from_millis(1), runtime_task).await???;

    let mut latest_saved = None;
    while let Ok(progress) = saved_rx.try_recv() {
        latest_saved = Some(progress.position_ms);
    }
    assert_eq!(
        latest_saved,
        Some(18),
        "accepted PlayerProgress was dropped when {label} cancelled lane admission"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn accepted_progress_ahead_of_keyboard_quit_survives_cancelled_durable_admission()
-> TestResult {
    assert_accepted_progress_survives_cancelled_durable_admission(
        RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        "keyboard quit",
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn accepted_progress_ahead_of_signal_survives_cancelled_durable_admission() -> TestResult {
    assert_accepted_progress_survives_cancelled_durable_admission(RuntimeEvent::Signal, "signal")
        .await
}

#[tokio::test(start_paused = true)]
async fn bounded_internal_work_backpressures_a_slow_account_lane() -> TestResult {
    let release = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_account(Arc::new(BlockingAccount {
            started: started_tx,
            release: release.clone(),
        }))
        .with_action_capacity(1);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        AcknowledgedEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let (first_ack_tx, first_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Action(Action::ConnectAccountRequested {
            browser: Browser::Chrome,
        }),
        first_ack_tx,
    ))?;
    first_ack_rx.await?;
    started_rx
        .recv()
        .await
        .ok_or("first account import did not start")?;

    let mut last_ack = None;
    for index in 0..32 {
        let browser = match index % 3 {
            0 => Browser::Chrome,
            1 => Browser::Firefox,
            _ => Browser::Safari,
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        event_tx.send((
            RuntimeEvent::Action(Action::ConnectAccountRequested { browser }),
            ack_tx,
        ))?;
        last_ack = Some(ack_rx);
    }
    assert!(
        tokio::time::timeout(
            Duration::from_millis(1),
            last_ack.ok_or("account burst was empty")?,
        )
        .await
        .is_err(),
        "slow account work was consumed into an unbounded internal backlog"
    );

    release.add_permits(33);
    let (quit_ack_tx, quit_ack_rx) = oneshot::channel();
    event_tx.send((RuntimeEvent::Quit, quit_ack_tx))?;
    quit_ack_rx.await?;
    runtime_task.await??;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_cancels_active_precommit_and_queued_account_work() -> TestResult {
    let release = Arc::new(Semaphore::new(0));
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_account(Arc::new(BlockingAccount {
            started: started_tx,
            release,
        }))
        .with_action_capacity(4)
        .with_shutdown_timeout(Duration::from_secs(1));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let mut runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        AcknowledgedEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let (first_ack_tx, first_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Action(Action::ConnectAccountRequested {
            browser: Browser::Firefox,
        }),
        first_ack_tx,
    ))?;
    first_ack_rx.await?;
    assert_eq!(started_rx.recv().await, Some(Browser::Firefox));

    let (second_ack_tx, second_ack_rx) = oneshot::channel();
    event_tx.send((
        RuntimeEvent::Action(Action::ConnectAccountRequested {
            browser: Browser::Chrome,
        }),
        second_ack_tx,
    ))?;
    second_ack_rx.await?;
    let (quit_ack_tx, quit_ack_rx) = oneshot::channel();
    event_tx.send((RuntimeEvent::Quit, quit_ack_tx))?;
    quit_ack_rx.await?;

    tokio::time::timeout(Duration::from_millis(1), &mut runtime_task)
        .await
        .map_err(|_| "shutdown waited for an active pre-commit account attempt")???;
    assert!(
        started_rx.try_recv().is_err(),
        "shutdown started a queued account import"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn replacement_search_cancels_superseded_provider_future() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(CancellingSearchProvider {
        started: started_tx,
        cancelled: cancelled_tx,
    });
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_provider(provider);
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::SearchSubmitted {
        query: "first".to_owned(),
        filter: SearchFilter::Songs,
    }))?;
    tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
        .await?
        .ok_or("first search did not start")?;

    event_tx.send(RuntimeEvent::Action(Action::SearchSubmitted {
        query: "second".to_owned(),
        filter: SearchFilter::Songs,
    }))?;
    tokio::time::timeout(Duration::from_secs(1), cancelled_rx.recv())
        .await?
        .ok_or("superseded search was not cancelled")?;

    let mut saw_second = false;
    for _ in 0..6 {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("runtime state stream closed")?;
        if state
            .search()
            .items()
            .iter()
            .any(|item| matches!(item, ytermusic::app::SearchItem::Playable(item) if item.id.video_id == "second-result"))
        {
            saw_second = true;
            break;
        }
    }
    assert!(saw_second);

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[tokio::test]
async fn superseding_chart_generations_keep_provider_work_bounded() -> TestResult {
    const REQUESTS: usize = 32;
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(SupersedingProvider {
        active: Arc::new(AtomicUsize::new(0)),
        chart_started: Some(started_tx),
        podcast_started: None,
    });
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_provider(provider);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let mut max_active = 0;
    for index in 0..REQUESTS {
        let region = if index % 2 == 0 { "HK" } else { "US" };
        event_tx.send(RuntimeEvent::Action(Action::ChartsRequested {
            region: RegionCode::parse(region)?,
        }))?;
        max_active = max_active.max(started_rx.recv().await.ok_or("chart start stream closed")?);
    }

    event_tx.send(RuntimeEvent::Quit)?;
    runtime_task.await??;
    assert_eq!(
        max_active, 1,
        "superseded chart generations retained {max_active} active provider calls"
    );
    Ok(())
}

#[tokio::test]
async fn superseding_podcast_generations_keep_provider_work_bounded() -> TestResult {
    const REQUESTS: usize = 32;
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(SupersedingProvider {
        active: Arc::new(AtomicUsize::new(0)),
        chart_started: None,
        podcast_started: Some(started_tx),
    });
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_provider(provider);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::SearchSubmitted {
        query: "bounded podcast".to_owned(),
        filter: SearchFilter::Podcasts,
    }))?;
    loop {
        let state = state_rx.recv().await.ok_or("runtime state stream closed")?;
        if !state.search().loading() && !state.search().items().is_empty() {
            break;
        }
    }

    let mut max_active = 0;
    for _ in 0..REQUESTS {
        event_tx.send(RuntimeEvent::Action(Action::OpenSelectedPodcast))?;
        max_active = max_active.max(
            started_rx
                .recv()
                .await
                .ok_or("podcast start stream closed")?,
        );
    }

    event_tx.send(RuntimeEvent::Quit)?;
    runtime_task.await??;
    assert_eq!(
        max_active, 1,
        "superseded podcast generations retained {max_active} active provider calls"
    );
    Ok(())
}

#[tokio::test]
async fn superseding_chart_cache_generations_keep_storage_work_bounded() -> TestResult {
    const REQUESTS: usize = 32;
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let storage = Arc::new(SupersedingChartCacheStorage {
        active: Arc::new(AtomicUsize::new(0)),
        started: started_tx,
    });
    let services = RuntimeServices::new(storage);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let mut max_active = 0;
    for index in 0..REQUESTS {
        let region = if index % 2 == 0 { "HK" } else { "US" };
        event_tx.send(RuntimeEvent::Action(Action::ChartsRequested {
            region: RegionCode::parse(region)?,
        }))?;
        max_active = max_active.max(
            started_rx
                .recv()
                .await
                .ok_or("chart-cache start stream closed")?,
        );
    }

    event_tx.send(RuntimeEvent::Quit)?;
    runtime_task.await??;
    assert_eq!(
        max_active, 1,
        "superseded chart-cache generations retained {max_active} active storage calls"
    );
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SearchCall {
    Initial(String),
    More(String),
}

#[tokio::test]
async fn search_more_uses_opaque_continuation_instead_of_replaying_first_page() -> TestResult {
    let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_provider(Arc::new(PaginationProvider { calls: calls_tx }));
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::SearchSubmitted {
        query: "paged".to_owned(),
        filter: SearchFilter::Songs,
    }))?;
    assert_eq!(
        calls_rx.recv().await.ok_or("search call stream closed")?,
        SearchCall::Initial("paged".to_owned())
    );
    loop {
        let state = state_rx.recv().await.ok_or("state stream closed")?;
        if state.search().continuation().is_some() && !state.search().loading() {
            break;
        }
    }

    event_tx.send(RuntimeEvent::Action(Action::SearchMoreRequested))?;
    assert_eq!(
        calls_rx.recv().await.ok_or("search call stream closed")?,
        SearchCall::More("next-page".to_owned())
    );

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn browser_selection_reaches_exact_runtime_account_boundary() -> TestResult {
    let (browser_tx, mut browser_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_account(Arc::new(RecordingAccount {
            selected: browser_tx,
        }));
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::ConnectAccountRequested {
        browser: Browser::Safari,
    }))?;
    assert_eq!(
        browser_rx.recv().await.ok_or("account boundary closed")?,
        Browser::Safari
    );

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn successful_account_import_atomically_swaps_next_library_request_provider() -> TestResult {
    let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
    let anonymous: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Unauthenticated,
        label: "anonymous",
        calls: calls_tx.clone(),
    });
    let authenticated: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Authenticated,
        label: "authenticated",
        calls: calls_tx,
    });
    let shared = Arc::new(SharedMusicProvider::new(anonymous));
    let importer = Arc::new(RecordingCredentialImporter::default());
    let account = RuntimeAccountService::with_importer(
        importer.clone(),
        Arc::new(StaticProviderFactory::success(authenticated)),
        shared.clone(),
    );

    assert_eq!(
        account.connect(Browser::Firefox).await?,
        AuthenticationState::Authenticated
    );
    let prepared = importer
        .prepared
        .lock()
        .map_err(|_| "prepared browser calls poisoned")?
        .clone();
    assert_eq!(prepared, [Browser::Firefox]);
    assert_eq!(importer.commits.load(Ordering::SeqCst), 1);

    let _ = shared.library(LibrarySection::Songs).await?;
    assert_eq!(
        calls_rx.recv().await.ok_or("provider call stream closed")?,
        "authenticated"
    );
    Ok(())
}

#[tokio::test]
async fn shared_provider_delegates_bounded_plain_lyrics_to_current_provider() -> TestResult {
    let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
    let provider: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Unauthenticated,
        label: "sentinel-provider-lyrics",
        calls: calls_tx,
    });
    let shared = SharedMusicProvider::new(provider);
    let media_id = MediaId {
        provider: "sentinel-provider".to_owned(),
        video_id: "sentinel-video-id".to_owned(),
    };

    let lyrics = shared.lyrics(&media_id).await?;

    assert_eq!(lyrics.text(), "sentinel-provider-lyrics");
    assert_eq!(
        calls_rx.recv().await.ok_or("provider call stream closed")?,
        "sentinel-provider-lyrics"
    );
    Ok(())
}

#[tokio::test]
async fn failed_account_import_keeps_old_provider_and_never_commits_or_leaks() -> TestResult {
    let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
    let anonymous: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Unauthenticated,
        label: "anonymous",
        calls: calls_tx,
    });
    let shared = Arc::new(SharedMusicProvider::new(anonymous));
    let importer = Arc::new(RecordingCredentialImporter::default());
    let account = RuntimeAccountService::with_importer(
        importer.clone(),
        Arc::new(StaticProviderFactory::failure()),
        shared.clone(),
    );

    let Err(error) = account.connect(Browser::Chrome).await else {
        return Err("provider construction unexpectedly succeeded".into());
    };
    assert_eq!(format!("{error:?}"), "RuntimeServiceError");
    assert!(!format!("{error:?}").contains("opaque-new-cookie"));
    assert_eq!(importer.commits.load(Ordering::SeqCst), 0);
    assert_eq!(
        shared.authentication(),
        AuthenticationState::Unauthenticated
    );

    let _ = shared.library(LibrarySection::Songs).await?;
    assert_eq!(
        calls_rx.recv().await.ok_or("provider call stream closed")?,
        "anonymous"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn overlapping_account_imports_are_serialized_through_commit_and_provider_swap() -> TestResult
{
    let (provider_calls_tx, mut provider_calls_rx) = mpsc::unbounded_channel();
    let anonymous: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Unauthenticated,
        label: "anonymous",
        calls: provider_calls_tx.clone(),
    });
    let first: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Authenticated,
        label: "first",
        calls: provider_calls_tx.clone(),
    });
    let second: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Authenticated,
        label: "second",
        calls: provider_calls_tx,
    });
    let shared = Arc::new(SharedMusicProvider::new(anonymous));
    let (import_events_tx, mut import_events_rx) = mpsc::unbounded_channel();
    let release_first_commit = Arc::new(Semaphore::new(0));
    let account = Arc::new(RuntimeAccountService::with_importer(
        Arc::new(BlockingFirstCommitImporter {
            events: import_events_tx,
            commit_calls: AtomicUsize::new(0),
            release_first_commit: release_first_commit.clone(),
        }),
        Arc::new(SequencedProviderFactory {
            providers: Mutex::new(VecDeque::from([first, second])),
        }),
        shared.clone(),
    ));

    let first_account = account.clone();
    let first_import = tokio::spawn(async move { first_account.connect(Browser::Firefox).await });
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::Prepared(Browser::Firefox))
    );
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::CommitStarted(0))
    );

    let (second_entered_tx, second_entered_rx) = oneshot::channel();
    let second_account = account;
    let second_import = tokio::spawn(async move {
        let _ = second_entered_tx.send(());
        second_account.connect(Browser::Chrome).await
    });
    second_entered_rx.await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(1), import_events_rx.recv())
            .await
            .is_err(),
        "second account import entered prepare before the first commit/provider swap completed"
    );

    release_first_commit.add_permits(1);
    assert_eq!(first_import.await??, AuthenticationState::Authenticated);
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::CommitFinished(0))
    );
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::Prepared(Browser::Chrome))
    );
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::CommitStarted(1))
    );
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::CommitFinished(1))
    );
    assert_eq!(second_import.await??, AuthenticationState::Authenticated);

    let _ = shared.library(LibrarySection::Songs).await?;
    assert_eq!(
        provider_calls_rx
            .recv()
            .await
            .ok_or("provider call stream closed")?,
        "second"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancelling_account_connect_after_commit_starts_still_completes_provider_swap() -> TestResult
{
    let (provider_calls_tx, mut provider_calls_rx) = mpsc::unbounded_channel();
    let anonymous: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Unauthenticated,
        label: "anonymous",
        calls: provider_calls_tx.clone(),
    });
    let authenticated: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Authenticated,
        label: "committed",
        calls: provider_calls_tx,
    });
    let shared = Arc::new(SharedMusicProvider::new(anonymous));
    let (import_events_tx, mut import_events_rx) = mpsc::unbounded_channel();
    let release_commit = Arc::new(Semaphore::new(0));
    let account = Arc::new(RuntimeAccountService::with_importer(
        Arc::new(BlockingFirstCommitImporter {
            events: import_events_tx,
            commit_calls: AtomicUsize::new(0),
            release_first_commit: release_commit.clone(),
        }),
        Arc::new(SequencedProviderFactory {
            providers: Mutex::new(VecDeque::from([authenticated])),
        }),
        shared.clone(),
    ));

    let task_account = account.clone();
    let connect = tokio::spawn(async move { task_account.connect(Browser::Safari).await });
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::Prepared(Browser::Safari))
    );
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::CommitStarted(0))
    );
    connect.abort();
    assert!(connect.await.is_err_and(|error| error.is_cancelled()));

    let mut shutdown = Box::pin(RuntimeAccount::shutdown(account.as_ref()));
    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut shutdown)
            .await
            .is_err(),
        "account shutdown lost ownership of the in-flight critical task"
    );
    release_commit.add_permits(1);
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(1), import_events_rx.recv())
            .await
            .map_err(|_| "commit was cancelled after entering its critical phase")?,
        Some(AccountImportEvent::CommitFinished(0))
    );
    shutdown.await;
    let _ = shared.library(LibrarySection::Songs).await?;
    assert_eq!(
        provider_calls_rx
            .recv()
            .await
            .ok_or("provider call stream closed")?,
        "committed"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn permanently_hung_account_commit_has_bounded_post_terminal_grace() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (provider_calls_tx, _provider_calls_rx) = mpsc::unbounded_channel();
    let anonymous: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Unauthenticated,
        label: "anonymous",
        calls: provider_calls_tx.clone(),
    });
    let authenticated: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Authenticated,
        label: "committed",
        calls: provider_calls_tx,
    });
    let shared = Arc::new(SharedMusicProvider::new(anonymous));
    let (import_events_tx, mut import_events_rx) = mpsc::unbounded_channel();
    let release_commit = Arc::new(Semaphore::new(0));
    let account = Arc::new(RuntimeAccountService::with_importer(
        Arc::new(BlockingFirstCommitImporter {
            events: import_events_tx,
            commit_calls: AtomicUsize::new(0),
            release_first_commit: release_commit.clone(),
        }),
        Arc::new(StaticProviderFactory::success(authenticated)),
        shared.clone(),
    ));
    let (shutdown_started_tx, mut shutdown_started_rx) = mpsc::unbounded_channel();
    let (restored_tx, mut restored_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(CleanupStorage {
        calls: Arc::clone(&calls),
    }))
    .with_account(account)
    .with_player(Arc::new(HungShutdownPlayer {
        calls: Arc::clone(&calls),
        shutdown_started: shutdown_started_tx,
    }))
    .with_terminal(Arc::new(RestorationNotifyingTerminal {
        terminal: RecordingTerminal {
            calls: Arc::clone(&calls),
        },
        restored: restored_tx,
    }))
    .with_shutdown_timeout(Duration::from_millis(25));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        RecordingRenderer {
            states: Arc::new(Mutex::new(Vec::new())),
        },
    ));

    event_tx.send(RuntimeEvent::Action(Action::ConnectAccountRequested {
        browser: Browser::Firefox,
    }))?;
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::Prepared(Browser::Firefox))
    );
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::CommitStarted(0))
    );
    event_tx.send(RuntimeEvent::Quit)?;

    tokio::time::timeout(Duration::from_millis(25), shutdown_started_rx.recv())
        .await
        .map_err(|_| "hung account commit blocked player shutdown beyond the cleanup deadline")?
        .ok_or("player shutdown notification stream closed")?;
    tokio::time::advance(Duration::from_millis(25)).await;
    tokio::time::timeout(Duration::from_millis(1), restored_rx.recv())
        .await
        .map_err(|_| "terminal restoration did not complete at the cleanup deadline")?
        .ok_or("terminal restoration notification stream closed")?;

    {
        let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
        let shutdown = calls
            .iter()
            .position(|call| *call == "player_shutdown")
            .ok_or("player shutdown missing")?;
        let abort = calls
            .iter()
            .position(|call| *call == "player_abort")
            .ok_or("hung player was not aborted")?;
        let restore = calls
            .iter()
            .position(|call| *call == "disable_raw")
            .ok_or("terminal was not restored")?;
        assert!(shutdown < abort);
        assert!(abort < restore, "cleanup order changed: {calls:?}");
    }
    assert_eq!(
        shared.authentication(),
        AuthenticationState::Unauthenticated
    );

    tokio::time::advance(Duration::from_millis(25)).await;
    tokio::time::timeout(Duration::from_millis(1), runtime_task)
        .await
        .map_err(|_| "runtime remained foregrounded after the post-terminal account grace")???;
    assert!(
        import_events_rx.try_recv().is_err(),
        "aborted commit reported completion after the transaction boundary"
    );
    assert_eq!(
        shared.authentication(),
        AuthenticationState::Unauthenticated
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn second_signal_forces_emergency_completion_after_terminal_restore() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (provider_calls_tx, _provider_calls_rx) = mpsc::unbounded_channel();
    let anonymous: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Unauthenticated,
        label: "anonymous",
        calls: provider_calls_tx.clone(),
    });
    let authenticated: Arc<dyn MusicProvider> = Arc::new(AccountProvider {
        authentication: AuthenticationState::Authenticated,
        label: "committed",
        calls: provider_calls_tx,
    });
    let shared = Arc::new(SharedMusicProvider::new(anonymous));
    let (import_events_tx, mut import_events_rx) = mpsc::unbounded_channel();
    let account = Arc::new(RuntimeAccountService::with_importer(
        Arc::new(BlockingFirstCommitImporter {
            events: import_events_tx,
            commit_calls: AtomicUsize::new(0),
            release_first_commit: Arc::new(Semaphore::new(0)),
        }),
        Arc::new(StaticProviderFactory::success(authenticated)),
        shared.clone(),
    ));
    let (restored_tx, mut restored_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(CleanupStorage {
        calls: Arc::clone(&calls),
    }))
    .with_account(account)
    .with_player(Arc::new(CleanupPlayer {
        calls: Arc::clone(&calls),
    }))
    .with_terminal(Arc::new(RestorationNotifyingTerminal {
        terminal: RecordingTerminal { calls },
        restored: restored_tx,
    }))
    .with_shutdown_timeout(Duration::from_secs(1));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        RecordingRenderer {
            states: Arc::new(Mutex::new(Vec::new())),
        },
    ));

    event_tx.send(RuntimeEvent::Action(Action::ConnectAccountRequested {
        browser: Browser::Firefox,
    }))?;
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::Prepared(Browser::Firefox))
    );
    assert_eq!(
        import_events_rx.recv().await,
        Some(AccountImportEvent::CommitStarted(0))
    );
    event_tx.send(RuntimeEvent::Quit)?;
    restored_rx
        .recv()
        .await
        .ok_or("terminal was not restored before account grace")?;

    let _ = event_tx.send(RuntimeEvent::Signal);
    tokio::time::timeout(Duration::from_millis(1), runtime_task)
        .await
        .map_err(|_| "second signal did not force emergency completion")???;
    assert_eq!(
        shared.authentication(),
        AuthenticationState::Unauthenticated
    );
    Ok(())
}

#[test]
fn production_provider_and_account_are_installed_as_one_runtime_pair() {
    let (calls, _calls_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(SharedMusicProvider::new(Arc::new(AccountProvider {
        authentication: AuthenticationState::Unauthenticated,
        label: "anonymous",
        calls,
    })));
    let (selected, _selected_rx) = mpsc::unbounded_channel();
    let account: Arc<dyn RuntimeAccount> = Arc::new(RecordingAccount { selected });

    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_account_provider(provider, account);

    assert!(services.account_configured());
}

#[tokio::test]
async fn runtime_artwork_decodes_two_by_two_pixels_and_reuses_url_size_cache() -> TestResult {
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = CountingArtworkFetcher {
        bytes: Arc::new(two_by_two_png()?),
        calls: calls.clone(),
        fail: false,
    };
    let store = Arc::new(ArtworkPresentationStore::new());
    let service = RuntimeArtworkService::new(
        fetcher,
        4,
        store.clone(),
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    );
    let url = artwork_url("https://images.example.test/signed.png?token=opaque")?;

    for generation in [Generation::new(11), Generation::new(12)] {
        service.request(generation, &url);
        RuntimeArtwork::fetch(&service, generation, url.clone()).await?;
    }

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let ArtworkPresentation::Grid(grid) = store
        .presentation(Generation::new(12), &url)
        .ok_or("current artwork presentation missing")?
    else {
        return Err("decoded artwork did not reach presentation store".into());
    };
    let left = grid.cell(0, 0).ok_or("left artwork cell missing")?;
    assert_eq!(left.foreground().red(), 255);
    assert_eq!(left.foreground().green(), 0);
    assert_eq!(left.background().blue(), 255);
    let right = grid.cell(1, 0).ok_or("right artwork cell missing")?;
    assert_eq!(right.foreground().green(), 255);
    assert_eq!(right.background().red(), 255);
    assert_eq!(right.background().green(), 255);
    assert_eq!(right.background().blue(), 255);
    Ok(())
}

#[tokio::test]
async fn runtime_artwork_failure_publishes_safe_fallback() -> TestResult {
    let store = Arc::new(ArtworkPresentationStore::new());
    let service = RuntimeArtworkService::new(
        CountingArtworkFetcher {
            bytes: Arc::new(Vec::new()),
            calls: Arc::new(AtomicUsize::new(0)),
            fail: true,
        },
        4,
        store.clone(),
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    );
    let generation = Generation::new(21);
    let url = artwork_url("https://images.example.test/unavailable.png")?;

    service.request(generation, &url);
    assert!(
        RuntimeArtwork::fetch(&service, generation, url.clone())
            .await
            .is_err()
    );
    let ArtworkPresentation::Fallback(fallback) = store
        .presentation(generation, &url)
        .ok_or("fallback presentation missing")?
    else {
        return Err("failed artwork did not publish fallback".into());
    };
    assert_eq!(fallback.icon(), "♪");
    assert_eq!(fallback.metadata(), "Artwork unavailable");
    Ok(())
}

#[tokio::test]
async fn stale_artwork_completion_cannot_replace_current_presentation() -> TestResult {
    let store = Arc::new(ArtworkPresentationStore::new());
    let service = RuntimeArtworkService::new(
        CountingArtworkFetcher {
            bytes: Arc::new(two_by_two_png()?),
            calls: Arc::new(AtomicUsize::new(0)),
            fail: false,
        },
        4,
        store.clone(),
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    );
    let stale_generation = Generation::new(30);
    let current_generation = Generation::new(31);
    let stale_url = artwork_url("https://images.example.test/stale.png")?;
    let current_url = artwork_url("https://images.example.test/current.png")?;

    service.request(stale_generation, &stale_url);
    service.request(current_generation, &current_url);
    RuntimeArtwork::fetch(&service, current_generation, current_url.clone()).await?;
    assert!(!store.publish(
        stale_generation,
        &stale_url,
        ArtworkPresentation::unavailable(),
    ));
    assert!(matches!(
        store.presentation(current_generation, &current_url),
        Some(ArtworkPresentation::Grid(_))
    ));
    Ok(())
}

#[tokio::test]
async fn production_artwork_components_share_one_store_between_boundary_and_renderer_handle()
-> TestResult {
    let components = ArtworkRuntimeComponents::new(
        CountingArtworkFetcher {
            bytes: Arc::new(two_by_two_png()?),
            calls: Arc::new(AtomicUsize::new(0)),
            fail: false,
        },
        4,
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    );
    let boundary = components.runtime_artwork();
    let renderer_store = components.presentation_store();
    let generation = Generation::new(41);
    let url = artwork_url("https://images.example.test/shared.png")?;

    boundary.request(generation, &url);
    boundary.fetch(generation, url.clone()).await?;

    assert!(matches!(
        renderer_store.presentation(generation, &url),
        Some(ArtworkPresentation::Grid(_))
    ));
    Ok(())
}

#[tokio::test]
async fn replacing_artwork_request_drops_superseded_fetch() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let store = Arc::new(ArtworkPresentationStore::new());
    let artwork = Arc::new(RuntimeArtworkService::new(
        CancellingArtworkFetcher {
            started: started_tx,
            cancelled: cancelled_tx,
            bytes: Arc::new(two_by_two_png()?),
        },
        4,
        store,
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    ));
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_artwork(artwork);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let stale_url = artwork_url("https://images.example.test/stale-blocked.png")?;
    let current_url = artwork_url("https://images.example.test/current-ready.png")?;

    event_tx.send(RuntimeEvent::Action(Action::ArtworkRequested {
        url: stale_url,
    }))?;
    started_rx.recv().await.ok_or("stale fetch did not start")?;
    event_tx.send(RuntimeEvent::Action(Action::ArtworkRequested {
        url: current_url,
    }))?;
    tokio::time::timeout(Duration::from_millis(250), cancelled_rx.recv())
        .await?
        .ok_or("superseded fetch cancellation stream closed")?;

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn art_to_no_art_identity_change_cancels_fetch_and_clears_presentation() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let store = Arc::new(ArtworkPresentationStore::new());
    let artwork = Arc::new(RuntimeArtworkService::new(
        CancellingArtworkFetcher {
            started: started_tx,
            cancelled: cancelled_tx,
            bytes: Arc::new(two_by_two_png()?),
        },
        4,
        store.clone(),
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    ));
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_artwork(artwork);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let stale_url = artwork_url("https://images.example.test/stale-blocked.png")?;

    event_tx.send(RuntimeEvent::Action(Action::ArtworkRequested {
        url: stale_url.clone(),
    }))?;
    started_rx
        .recv()
        .await
        .ok_or("artwork fetch did not start")?;
    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: song("no-art-identity"),
    }))?;
    tokio::time::timeout(Duration::from_millis(250), cancelled_rx.recv())
        .await?
        .ok_or("art-to-no-art transition did not cancel the fetch")?;
    assert!(
        store.presentation(Generation::new(1), &stale_url).is_none(),
        "art-to-no-art transition retained the stale presentation"
    );

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn active_search_error_cancels_fetch_and_clears_presentation() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let store = Arc::new(ArtworkPresentationStore::new());
    let artwork = Arc::new(RuntimeArtworkService::new(
        CancellingArtworkFetcher {
            started: started_tx,
            cancelled: cancelled_tx,
            bytes: Arc::new(two_by_two_png()?),
        },
        4,
        store.clone(),
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    ));
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_artwork(artwork);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let stale_url = artwork_url("https://images.example.test/stale-blocked.png")?;

    event_tx.send(RuntimeEvent::Action(Action::ArtworkSurfaceChanged {
        surface: ytermusic::app::ArtworkSurface::Search,
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::ArtworkRequested {
        url: stale_url.clone(),
    }))?;
    started_rx
        .recv()
        .await
        .ok_or("artwork fetch did not start")?;
    event_tx.send(RuntimeEvent::Action(Action::SearchSubmitted {
        query: "active-error".to_owned(),
        filter: SearchFilter::Songs,
    }))?;
    tokio::time::timeout(Duration::from_millis(250), cancelled_rx.recv())
        .await?
        .ok_or("active search error did not cancel the old artwork fetch")?;
    assert!(
        store.presentation(Generation::new(1), &stale_url).is_none(),
        "active search error retained the stale presentation"
    );

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn unrelated_search_does_not_cancel_current_artwork_fetch() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let (search_started_tx, mut search_started_rx) = mpsc::unbounded_channel();
    let (search_cancelled_tx, _search_cancelled_rx) = mpsc::unbounded_channel();
    let artwork = Arc::new(RuntimeArtworkService::new(
        CancellingArtworkFetcher {
            started: started_tx,
            cancelled: cancelled_tx,
            bytes: Arc::new(two_by_two_png()?),
        },
        4,
        Arc::new(ArtworkPresentationStore::new()),
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    ));
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_provider(Arc::new(CancellingSearchProvider {
            started: search_started_tx,
            cancelled: search_cancelled_tx,
        }))
        .with_artwork(artwork);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::ArtworkRequested {
        url: artwork_url("https://images.example.test/stale-blocked.png")?,
    }))?;
    started_rx
        .recv()
        .await
        .ok_or("artwork fetch did not start")?;
    event_tx.send(RuntimeEvent::Action(Action::SearchSubmitted {
        query: "first".to_owned(),
        filter: SearchFilter::All,
    }))?;
    search_started_rx
        .recv()
        .await
        .ok_or("unrelated search did not start")?;
    assert!(
        tokio::time::timeout(Duration::from_millis(25), cancelled_rx.recv())
            .await
            .is_err(),
        "search unexpectedly cancelled the active artwork request"
    );

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    cancelled_rx
        .recv()
        .await
        .ok_or("shutdown did not cancel artwork fetch")?;
    Ok(())
}

#[tokio::test]
async fn shutdown_drops_pending_artwork_fetch() -> TestResult {
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let (cancelled_tx, mut cancelled_rx) = mpsc::unbounded_channel();
    let artwork = Arc::new(RuntimeArtworkService::new(
        CancellingArtworkFetcher {
            started: started_tx,
            cancelled: cancelled_tx,
            bytes: Arc::new(two_by_two_png()?),
        },
        4,
        Arc::new(ArtworkPresentationStore::new()),
        CellSize::new(2, 1),
        ColorCapability::TrueColor,
    ));
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_artwork(artwork);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::ArtworkRequested {
        url: artwork_url("https://images.example.test/stale-blocked.png")?,
    }))?;
    started_rx
        .recv()
        .await
        .ok_or("artwork fetch did not start")?;
    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;

    tokio::time::timeout(Duration::from_millis(250), cancelled_rx.recv())
        .await?
        .ok_or("pending artwork cancellation stream closed")?;
    Ok(())
}

#[tokio::test]
async fn player_boundary_failure_returns_to_reducer_as_diagnostic() -> TestResult {
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(FailingPlayer));
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::TargetVolumeChanged(23)))?;
    tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            let state = state_rx.recv().await.ok_or("state stream closed")?;
            if state.diagnostics().iter().any(|diagnostic| {
                diagnostic.category() == DiagnosticCategory::State
                    && diagnostic.message() == "player command failed"
            }) {
                return Ok::<(), Box<dyn Error>>(());
            }
        }
    })
    .await??;

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn seek_boundary_failure_is_nonfatal_and_bounded_to_a_diagnostic() -> TestResult {
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(AcceptingPlayer));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let item = song("failed-seek-command");

    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: item.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&item.id),
    }))?;
    let generation = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let state = state_rx.recv().await.ok_or("state stream closed")?;
            if let Some(generation) = state.current_attempt_generation() {
                return Ok::<_, Box<dyn Error>>(generation);
            }
        }
    })
    .await??;

    event_tx.send(RuntimeEvent::Action(Action::PlayerProgress {
        generation,
        media_id: item.id,
        position_ms: 30_000,
        duration_ms: item.duration_ms,
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::SeekRelativeRequested {
        seconds: 10,
    }))?;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let state = state_rx.recv().await.ok_or("state stream closed")?;
            if state.diagnostics().iter().any(|diagnostic| {
                diagnostic.category() == DiagnosticCategory::State
                    && diagnostic.message() == "player command failed"
            }) {
                return Ok::<(), Box<dyn Error>>(());
            }
        }
    })
    .await??;

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

struct RecordingAccount {
    selected: mpsc::UnboundedSender<Browser>,
}

struct BlockingAccount {
    started: mpsc::UnboundedSender<Browser>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl RuntimeAccount for BlockingAccount {
    async fn connect(&self, browser: Browser) -> Result<AuthenticationState, RuntimeServiceError> {
        self.started
            .send(browser)
            .map_err(|_| RuntimeServiceError)?;
        self.release
            .acquire()
            .await
            .map_err(|_| RuntimeServiceError)?
            .forget();
        Ok(AuthenticationState::Authenticated)
    }
}

#[derive(Default)]
struct RecordingCredentialImporter {
    prepared: Mutex<Vec<Browser>>,
    commits: AtomicUsize,
}

#[async_trait]
impl RuntimeCredentialImporter for RecordingCredentialImporter {
    async fn prepare(&self, browser: Browser) -> Result<SecretString, RuntimeServiceError> {
        self.prepared
            .lock()
            .map_err(|_| RuntimeServiceError)?
            .push(browser);
        Ok(SecretString::from("opaque-new-cookie".to_owned()))
    }

    async fn commit(&self, _credential: SecretString) -> Result<(), RuntimeServiceError> {
        self.commits.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct StaticProviderFactory {
    provider: Option<Arc<dyn MusicProvider>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AccountImportEvent {
    Prepared(Browser),
    CommitStarted(usize),
    CommitFinished(usize),
}

struct BlockingFirstCommitImporter {
    events: mpsc::UnboundedSender<AccountImportEvent>,
    commit_calls: AtomicUsize,
    release_first_commit: Arc<Semaphore>,
}

#[async_trait]
impl RuntimeCredentialImporter for BlockingFirstCommitImporter {
    async fn prepare(&self, browser: Browser) -> Result<SecretString, RuntimeServiceError> {
        self.events
            .send(AccountImportEvent::Prepared(browser))
            .map_err(|_| RuntimeServiceError)?;
        Ok(SecretString::from("opaque-account-cookie".to_owned()))
    }

    async fn commit(&self, _credential: SecretString) -> Result<(), RuntimeServiceError> {
        let call = self.commit_calls.fetch_add(1, Ordering::SeqCst);
        self.events
            .send(AccountImportEvent::CommitStarted(call))
            .map_err(|_| RuntimeServiceError)?;
        if call == 0 {
            self.release_first_commit
                .acquire()
                .await
                .map_err(|_| RuntimeServiceError)?
                .forget();
        }
        self.events
            .send(AccountImportEvent::CommitFinished(call))
            .map_err(|_| RuntimeServiceError)
    }
}

struct SequencedProviderFactory {
    providers: Mutex<VecDeque<Arc<dyn MusicProvider>>>,
}

#[async_trait]
impl AuthenticatedProviderFactory for SequencedProviderFactory {
    async fn create(&self, _cookie: &SecretString) -> ProviderResult<Arc<dyn MusicProvider>> {
        self.providers
            .lock()
            .map_err(|_| unavailable(ProviderOperation::Library))?
            .pop_front()
            .ok_or_else(|| unavailable(ProviderOperation::Library))
    }
}

impl StaticProviderFactory {
    fn success(provider: Arc<dyn MusicProvider>) -> Self {
        Self {
            provider: Some(provider),
        }
    }

    const fn failure() -> Self {
        Self { provider: None }
    }
}

#[async_trait]
impl AuthenticatedProviderFactory for StaticProviderFactory {
    async fn create(&self, _cookie: &SecretString) -> ProviderResult<Arc<dyn MusicProvider>> {
        self.provider
            .clone()
            .ok_or_else(|| unavailable(ProviderOperation::Library))
    }
}

struct AccountProvider {
    authentication: AuthenticationState,
    label: &'static str,
    calls: mpsc::UnboundedSender<&'static str>,
}

struct CountingArtworkFetcher {
    bytes: Arc<Vec<u8>>,
    calls: Arc<AtomicUsize>,
    fail: bool,
}

struct CancellingArtworkFetcher {
    started: mpsc::UnboundedSender<()>,
    cancelled: mpsc::UnboundedSender<()>,
    bytes: Arc<Vec<u8>>,
}

#[async_trait]
impl ArtworkFetcher for CountingArtworkFetcher {
    async fn fetch(&self, _url: &url::Url) -> Result<ArtworkByteStream, ArtworkFetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(ArtworkFetchError::unavailable());
        }
        Ok(Box::pin(stream::iter([Ok(Bytes::from(
            self.bytes.as_ref().clone(),
        ))])))
    }
}

#[async_trait]
impl ArtworkFetcher for CancellingArtworkFetcher {
    async fn fetch(&self, url: &url::Url) -> Result<ArtworkByteStream, ArtworkFetchError> {
        if url.path().contains("stale-blocked") {
            self.started
                .send(())
                .map_err(|_| ArtworkFetchError::unavailable())?;
            let notice = DropNotice {
                cancelled: self.cancelled.clone(),
            };
            return Ok(Box::pin(stream::once(async move {
                let _notice = notice;
                future::pending::<Result<Bytes, ArtworkFetchError>>().await
            })));
        }
        Ok(Box::pin(stream::iter([Ok(Bytes::from(
            self.bytes.as_ref().clone(),
        ))])))
    }
}

fn artwork_url(value: &str) -> Result<ArtworkUrl, Box<dyn Error>> {
    Ok(ArtworkUrl::try_from(url::Url::parse(value)?)?)
}

fn two_by_two_png() -> Result<Vec<u8>, Box<dyn Error>> {
    let image = RgbaImage::from_fn(2, 2, |x, y| match (x, y) {
        (0, 0) => Rgba([255, 0, 0, 255]),
        (1, 0) => Rgba([0, 255, 0, 255]),
        (0, 1) => Rgba([0, 0, 255, 255]),
        (1, 1) => Rgba([255, 255, 255, 255]),
        _ => Rgba([0, 0, 0, 255]),
    });
    let mut output = io::Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image).write_to(&mut output, ImageFormat::Png)?;
    Ok(output.into_inner())
}

#[async_trait]
impl MusicProvider for AccountProvider {
    async fn search(
        &self,
        _query: &str,
        _filter: SearchFilter,
    ) -> ProviderResult<Page<SearchItem>> {
        Err(unavailable(ProviderOperation::Search))
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        Err(unavailable(ProviderOperation::Charts))
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Playlist))
    }

    async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
        Err(unavailable(ProviderOperation::Podcast))
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Radio))
    }

    async fn lyrics(&self, _id: &MediaId) -> ProviderResult<PlainLyrics> {
        self.calls
            .send(self.label)
            .map_err(|_| unavailable(ProviderOperation::Lyrics))?;
        PlainLyrics::new(self.label)
    }

    async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        self.calls
            .send(self.label)
            .map_err(|_| unavailable(ProviderOperation::Library))?;
        Ok(Page {
            items: Vec::new(),
            continuation: None,
            stale: false,
        })
    }

    fn authentication(&self) -> AuthenticationState {
        self.authentication
    }
}

#[async_trait]
impl RuntimeAccount for RecordingAccount {
    async fn connect(&self, browser: Browser) -> Result<AuthenticationState, RuntimeServiceError> {
        self.selected
            .send(browser)
            .map_err(|_| RuntimeServiceError)?;
        Ok(AuthenticationState::Authenticated)
    }
}

#[tokio::test]
async fn player_actions_return_through_the_same_runtime_bus() -> TestResult {
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player_actions(Box::new(OnePlayerAction {
            action: Some(Action::TargetVolumeChanged(36)),
        }));
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    loop {
        let state = state_rx.recv().await.ok_or("state stream closed")?;
        if state.playback().target_volume == 36 {
            break;
        }
    }

    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn keyboard_quit_is_driven_by_the_ui_controller() -> TestResult {
    let runtime = Runtime::new(
        Config::default(),
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })),
    );
    tokio::time::timeout(
        Duration::from_millis(100),
        runtime.run(
            OneThenPending::new(RuntimeEvent::Key(KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
            ))),
            RecordingRenderer {
                states: Arc::new(Mutex::new(Vec::new())),
            },
        ),
    )
    .await??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn storage_calls_run_off_current_thread_executor() -> TestResult {
    let executor_thread = thread::current().id();
    let (called_tx, called_rx) = std_mpsc::channel();
    let storage = FifoStorage::spawn(Box::new(ThreadRecordingStorage {
        called: called_tx,
        favorite_events: None,
    }))?;

    assert!(storage.load_session().await?.is_none());
    let storage_thread = called_rx.try_recv()?;
    assert_ne!(storage_thread, executor_thread);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn favorites_fifo_storage_preserves_command_order() -> TestResult {
    let (called_tx, _called_rx) = std_mpsc::channel();
    let (events_tx, events_rx) = std_mpsc::channel();
    let storage = FifoStorage::spawn(Box::new(ThreadRecordingStorage {
        called: called_tx,
        favorite_events: Some(events_tx),
    }))?;
    let item = song("fifo-favorite");
    let id = item.id.clone();

    let (loaded, added, removed) = tokio::join!(
        biased;
        storage.load_favorites(),
        storage.add_favorite(item, 55),
        storage.remove_favorite(id),
    );
    assert!(loaded?.is_empty());
    assert_eq!(added?, FavoriteInsertOutcome::Added);
    assert!(!removed?);
    assert_eq!(
        events_rx.try_iter().collect::<Vec<_>>(),
        ["load", "add", "remove"]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_chart_cache_calls_do_not_leave_an_unbounded_sync_backlog() -> TestResult {
    const REQUESTS: usize = 32;
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let storage = Arc::new(FifoStorage::spawn(Box::new(BlockingSyncChartStorage {
        gate: Arc::clone(&gate),
        calls: Arc::clone(&calls),
    }))?);
    let barrier = Arc::new(tokio::sync::Barrier::new(REQUESTS + 1));
    let (polled_tx, mut polled_rx) = mpsc::unbounded_channel();
    let mut tasks = Vec::new();

    for index in 0..REQUESTS {
        let storage = Arc::clone(&storage);
        let barrier = Arc::clone(&barrier);
        let polled = polled_tx.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let key = format!("superseded-region-{index}");
            let mut load = Box::pin(storage.load_chart_cache(&key));
            assert!(futures::poll!(&mut load).is_pending());
            let _ = polled.send(());
            let _ = load.await;
        }));
    }
    drop(polled_tx);
    barrier.wait().await;
    for _ in 0..REQUESTS {
        polled_rx
            .recv()
            .await
            .ok_or("chart-cache poll observer closed")?;
    }
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }

    {
        let (released, wake) = &*gate;
        *released.lock().map_err(|_| "sync storage gate poisoned")? = true;
        wake.notify_one();
    }
    storage.load_chart_cache("sentinel").await?;
    assert!(
        calls.load(Ordering::SeqCst) <= 6,
        "cancelled chart-cache calls left {} synchronous commands queued",
        calls.load(Ordering::SeqCst)
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn session_checkpoints_are_debounced_to_latest_state() -> TestResult {
    let (saved_tx, mut saved_rx) = mpsc::unbounded_channel();
    let storage = Arc::new(RecordingRuntimeStorage { saved: saved_tx });
    let services = RuntimeServices::new(storage).with_clock(Arc::new(FixedClock(7_777)));
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::TargetVolumeChanged(31)))?;
    event_tx.send(RuntimeEvent::Action(Action::TargetVolumeChanged(47)))?;
    loop {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("runtime state stream closed")?;
        if state.playback().target_volume == 47 {
            break;
        }
    }

    tokio::time::advance(Duration::from_millis(249)).await;
    tokio::task::yield_now().await;
    assert!(saved_rx.try_recv().is_err());
    tokio::time::advance(Duration::from_millis(1)).await;
    let (checkpoint, updated_at) = tokio::time::timeout(Duration::from_secs(1), saved_rx.recv())
        .await?
        .ok_or("debounced checkpoint was not saved")?;
    assert_eq!(checkpoint.playback.target_volume, 47);
    assert_eq!(updated_at, 7_777);
    assert!(saved_rx.try_recv().is_err());

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PodcastStorageEvent {
    Save(PodcastProgress),
    Load(PodcastProgress),
}

#[tokio::test(start_paused = true)]
async fn podcast_writes_and_loads_preserve_fifo_vector_order_and_playback_epoch() -> TestResult {
    let episode = podcast_episode("fifo-episode");
    let mut checkpoint = checkpoint_for(episode.clone(), PlaybackStatus::Stopped);
    checkpoint.queue.repeat = RepeatMode::One;
    let initial_progress = PodcastProgress {
        video_id: episode.id.video_id.clone(),
        playback_epoch: 7,
        position_ms: 45_000,
        duration_ms: episode.duration_ms,
        played: false,
        updated_at: 100,
    };
    let (storage_tx, mut storage_rx) = mpsc::unbounded_channel();
    let storage = Arc::new(OrderedPodcastStorage {
        checkpoint,
        progress: Mutex::new(Some(initial_progress)),
        events: storage_tx,
    });
    let services = RuntimeServices::new(storage)
        .with_clock(Arc::new(FixedClock(7_777)))
        .with_player(Arc::new(AcceptingPlayer));
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::TogglePlayback))?;
    let attempt_generation = loop {
        let state = tokio::time::timeout(Duration::from_secs(1), state_rx.recv())
            .await?
            .ok_or("runtime state stream closed")?;
        if let Some(generation) = state.current_attempt_generation() {
            break generation;
        }
    };
    event_tx.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation: attempt_generation,
        status: PlaybackStatus::Playing,
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayerEnded {
        generation: attempt_generation,
    }))?;

    let mut observed = Vec::new();
    while observed.len() < 4 {
        observed.push(
            tokio::time::timeout(Duration::from_secs(1), storage_rx.recv())
                .await?
                .ok_or("podcast storage stream closed")?,
        );
    }
    let completion_save_index = observed
        .iter()
        .position(|event| matches!(event, PodcastStorageEvent::Save(progress) if progress.played))
        .ok_or("podcast completion was not saved")?;
    let PodcastStorageEvent::Save(completed) = &observed[completion_save_index] else {
        return Err("completion event kind changed".into());
    };
    let PodcastStorageEvent::Load(reloaded) = observed
        .get(completion_save_index + 1)
        .ok_or("completion save was not followed by a load")?
    else {
        return Err("podcast completion save/load order was reversed".into());
    };
    assert_eq!(completed.video_id, episode.id.video_id);
    assert_eq!(completed.playback_epoch, 8);
    assert_eq!(completed.position_ms, 180_000);
    assert_eq!(completed.duration_ms, episode.duration_ms);
    assert!(completed.played);
    assert_eq!(completed.updated_at, 7_777);
    assert_eq!(reloaded, completed);

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_flushes_final_checkpoint() -> TestResult {
    let (saved_tx, mut saved_rx) = mpsc::unbounded_channel();
    let storage = Arc::new(RecordingRuntimeStorage { saved: saved_tx });
    let services = RuntimeServices::new(storage).with_clock(Arc::new(FixedClock(8_888)));
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, _state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    event_tx.send(RuntimeEvent::Action(Action::TargetVolumeChanged(55)))?;
    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;

    let (checkpoint, updated_at) = saved_rx
        .try_recv()
        .map_err(|_| "shutdown did not flush the final checkpoint")?;
    assert_eq!(checkpoint.playback.target_volume, 55);
    assert_eq!(updated_at, 8_888);
    assert!(saved_rx.try_recv().is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_player_action_behind_quit_is_drained_before_final_checkpoint() -> TestResult {
    let (saved_tx, mut saved_rx) = mpsc::unbounded_channel();
    let (event_finished_tx, event_finished_rx) = oneshot::channel();
    let (action_accepted_tx, action_accepted_rx) = oneshot::channel();
    let (render_started_tx, mut render_started_rx) = mpsc::unbounded_channel();
    let render_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let services = RuntimeServices::new(Arc::new(RecordingRuntimeStorage { saved: saved_tx }))
        .with_clock(Arc::new(FixedClock(9_999)))
        .with_action_capacity(4)
        .with_player_actions(Box::new(ActionAfterEventTask {
            event_finished: Some(event_finished_rx),
            actions: VecDeque::from([Action::TargetVolumeChanged(73)]),
            action_accepted: Some(action_accepted_tx),
        }));
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        DropSignalledEvents {
            events: VecDeque::from([RuntimeEvent::Quit]),
            finished: Some(event_finished_tx),
        },
        BlockingFirstRenderer {
            started: render_started_tx,
            gate: Arc::clone(&render_gate),
            first: true,
        },
    ));

    tokio::time::timeout(Duration::from_secs(1), render_started_rx.recv())
        .await?
        .ok_or("runtime never entered its first render")?;
    tokio::time::timeout(Duration::from_secs(1), action_accepted_rx)
        .await?
        .map_err(|_| "player action producer stopped before enqueueing")?;

    {
        let (released, wake) = &*render_gate;
        *released.lock().map_err(|_| "render gate poisoned")? = true;
        wake.notify_one();
    }
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;

    let mut final_checkpoint = None;
    while let Ok((checkpoint, _)) = saved_rx.try_recv() {
        final_checkpoint = Some(checkpoint);
    }
    assert_eq!(
        final_checkpoint
            .ok_or("shutdown did not write a final checkpoint")?
            .playback
            .target_volume,
        73,
        "an action accepted behind Quit was discarded before the final checkpoint"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_action_behind_signal_preserves_durable_fifo_before_final_checkpoint() -> TestResult
{
    let item = song("drained-history");
    let queue_id = stable_queue_item_id(&item.id);
    let (prepared, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia { item: item.clone() },
    );
    let (prepared, _) = reduce(
        prepared,
        Action::PlayQueueItem {
            id: queue_id.clone(),
        },
    );
    let generation = prepared
        .current_attempt_generation()
        .ok_or("test playback did not start")?;
    let (storage_event_tx, mut storage_event_rx) = mpsc::unbounded_channel();
    let (event_finished_tx, event_finished_rx) = oneshot::channel();
    let (action_accepted_tx, action_accepted_rx) = oneshot::channel();
    let (render_started_tx, mut render_started_rx) = mpsc::unbounded_channel();
    let render_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let services = RuntimeServices::new(Arc::new(DrainRecordingStorage {
        events: storage_event_tx,
    }))
    .with_clock(Arc::new(FixedClock(10_001)))
    .with_player(Arc::new(AcceptingPlayer))
    .with_initial_action(Action::EnqueueMedia { item: item.clone() })
    .with_initial_action(Action::PlayQueueItem { id: queue_id })
    .with_action_capacity(4)
    .with_player_actions(Box::new(ActionAfterEventTask {
        event_finished: Some(event_finished_rx),
        actions: VecDeque::from([Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        }]),
        action_accepted: Some(action_accepted_tx),
    }));
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        DropSignalledEvents {
            events: VecDeque::from([RuntimeEvent::Signal]),
            finished: Some(event_finished_tx),
        },
        BlockingFirstRenderer {
            started: render_started_tx,
            gate: Arc::clone(&render_gate),
            first: true,
        },
    ));

    tokio::time::timeout(Duration::from_secs(1), render_started_rx.recv())
        .await?
        .ok_or("runtime never entered its first render")?;
    tokio::time::timeout(Duration::from_secs(1), action_accepted_rx)
        .await?
        .map_err(|_| "player action producer stopped before enqueueing")?;
    {
        let (released, wake) = &*render_gate;
        *released.lock().map_err(|_| "render gate poisoned")? = true;
        wake.notify_one();
    }
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;

    let mut storage_events = Vec::new();
    while let Ok(event) = storage_event_rx.try_recv() {
        storage_events.push(event);
    }
    let history_index = storage_events
        .iter()
        .position(
            |event| matches!(event, DrainStorageEvent::History(id) if id == "drained-history"),
        )
        .ok_or("accepted history write was not drained")?;
    let final_session_index = storage_events
        .iter()
        .rposition(|event| matches!(event, DrainStorageEvent::Session))
        .ok_or("final session checkpoint was not written")?;
    assert!(
        history_index < final_session_index,
        "durable effects were reordered after terminal drain"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_durable_drain_cannot_exceed_the_cleanup_deadline() -> TestResult {
    let item = song("hung-drained-history");
    let queue_id = stable_queue_item_id(&item.id);
    let (prepared, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia { item: item.clone() },
    );
    let (prepared, _) = reduce(
        prepared,
        Action::PlayQueueItem {
            id: queue_id.clone(),
        },
    );
    let generation = prepared
        .current_attempt_generation()
        .ok_or("test playback did not start")?;
    let (history_started_tx, mut history_started_rx) = mpsc::unbounded_channel();
    let (event_finished_tx, event_finished_rx) = oneshot::channel();
    let (action_accepted_tx, action_accepted_rx) = oneshot::channel();
    let (render_started_tx, mut render_started_rx) = mpsc::unbounded_channel();
    let render_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let services = RuntimeServices::new(Arc::new(HungHistoryStorage {
        started: history_started_tx,
    }))
    .with_player(Arc::new(AcceptingPlayer))
    .with_initial_action(Action::EnqueueMedia { item })
    .with_initial_action(Action::PlayQueueItem { id: queue_id })
    .with_action_capacity(4)
    .with_shutdown_timeout(Duration::from_millis(50))
    .with_player_actions(Box::new(ActionAfterEventTask {
        event_finished: Some(event_finished_rx),
        actions: VecDeque::from([Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        }]),
        action_accepted: Some(action_accepted_tx),
    }));
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        DropSignalledEvents {
            events: VecDeque::from([RuntimeEvent::Quit]),
            finished: Some(event_finished_tx),
        },
        BlockingFirstRenderer {
            started: render_started_tx,
            gate: Arc::clone(&render_gate),
            first: true,
        },
    ));

    tokio::time::timeout(Duration::from_secs(1), render_started_rx.recv())
        .await?
        .ok_or("runtime never entered its first render")?;
    tokio::time::timeout(Duration::from_secs(1), action_accepted_rx)
        .await?
        .map_err(|_| "player action producer stopped before enqueueing")?;
    {
        let (released, wake) = &*render_gate;
        *released.lock().map_err(|_| "render gate poisoned")? = true;
        wake.notify_one();
    }
    tokio::time::timeout(Duration::from_secs(1), history_started_rx.recv())
        .await?
        .ok_or("drained history write never started")?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task)
        .await
        .map_err(|_| "hung durable drain exceeded the cleanup deadline")???;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(runtime_panic_hook)]
async fn accepted_panic_behind_keyboard_quit_upgrades_the_terminal_outcome() -> TestResult {
    let (events_finished_tx, events_finished_rx) = oneshot::channel();
    let (render_started_tx, mut render_started_rx) = mpsc::unbounded_channel();
    let render_gate = Arc::new((Mutex::new(false), Condvar::new()));
    let runtime_task = tokio::spawn(
        Runtime::new(
            Config::default(),
            RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })),
        )
        .run(
            DropSignalledEvents {
                events: VecDeque::from([
                    RuntimeEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
                    RuntimeEvent::Panic,
                ]),
                finished: Some(events_finished_tx),
            },
            BlockingFirstRenderer {
                started: render_started_tx,
                gate: Arc::clone(&render_gate),
                first: true,
            },
        ),
    );

    tokio::time::timeout(Duration::from_secs(1), render_started_rx.recv())
        .await?
        .ok_or("runtime never entered its first render")?;
    tokio::time::timeout(Duration::from_secs(1), events_finished_rx)
        .await?
        .map_err(|_| "event producer stopped before accepting the panic")?;
    {
        let (released, wake) = &*render_gate;
        *released.lock().map_err(|_| "render gate poisoned")? = true;
        wake.notify_one();
    }

    let joined = tokio::time::timeout(Duration::from_secs(1), runtime_task).await?;
    assert!(
        joined.is_err_and(|error| error.is_panic()),
        "an accepted panic behind keyboard quit did not upgrade the terminal outcome"
    );
    Ok(())
}

#[test]
fn resume_offset_is_applied_exactly_once() -> TestResult {
    let episode = podcast_episode("single-resume-offset");
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let progress_generation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::LoadPodcastProgress { generation, .. } => Some(*generation),
            _ => None,
        })
        .ok_or("podcast did not request saved progress")?;
    let (state, effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: Some(PodcastProgress {
                video_id: episode.id.video_id.clone(),
                playback_epoch: 7,
                position_ms: 65_000,
                duration_ms: episode.duration_ms,
                played: false,
                updated_at: 1,
            }),
        },
    );
    let (playback_generation, start_ms) = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Resolve {
                generation,
                start_ms,
                ..
            } => Some((*generation, *start_ms)),
            _ => None,
        })
        .ok_or("podcast did not request resolution")?;
    assert_eq!(start_ms, Some(65_000));

    let (_, post_load_effects) = reduce(
        state,
        Action::ResolveSucceeded {
            generation: playback_generation,
        },
    );
    assert!(post_load_effects.is_empty());
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
enum PlayerCall {
    Play {
        generation: ytermusic::app::Generation,
        item: Box<MediaItem>,
        start_ms: Option<u64>,
    },
    Pause,
    Resume,
    Volume(u8),
    SeekRelative(i64),
}

#[tokio::test(start_paused = true)]
async fn every_emitted_player_command_maps() -> TestResult {
    let (player_tx, mut player_rx) = mpsc::unbounded_channel();
    let player = Arc::new(RecordingPlayer { calls: player_tx });
    let services =
        RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None })).with_player(player);
    let runtime = Runtime::new(Config::default(), services);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(runtime.run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let item = song("mapped-player-command");

    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: item.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&item.id),
    }))?;
    let PlayerCall::Play {
        generation,
        item: played_item,
        start_ms,
    } = tokio::time::timeout(Duration::from_secs(1), player_rx.recv())
        .await?
        .ok_or("player call stream closed")?
    else {
        return Err("Resolve did not map to player play".into());
    };
    assert_eq!(*played_item, item);
    assert_eq!(start_ms, None);

    event_tx.send(RuntimeEvent::Action(Action::ResolveSucceeded {
        generation,
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Playing,
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::TogglePlayback))?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), player_rx.recv())
            .await?
            .ok_or("player call stream closed")?,
        PlayerCall::Pause
    );

    event_tx.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Paused,
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::TogglePlayback))?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), player_rx.recv())
            .await?
            .ok_or("player call stream closed")?,
        PlayerCall::Resume
    );

    event_tx.send(RuntimeEvent::Action(Action::TargetVolumeChanged(44)))?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), player_rx.recv())
            .await?
            .ok_or("player call stream closed")?,
        PlayerCall::Volume(44)
    );

    event_tx.send(RuntimeEvent::Action(Action::PlayerProgress {
        generation,
        media_id: item.id.clone(),
        position_ms: 30_000,
        duration_ms: item.duration_ms,
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::SeekRelativeRequested {
        seconds: 10,
    }))?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), player_rx.recv())
            .await?
            .ok_or("player call stream closed")?,
        PlayerCall::SeekRelative(10)
    );

    while state_rx.try_recv().is_ok() {}
    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_secs(1), runtime_task).await???;
    Ok(())
}

#[tokio::test]
async fn runtime_dispatches_now_playing_through_injected_notifier() -> TestResult {
    let (notification_tx, mut notification_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(AcceptingPlayer))
        .with_notifier(Arc::new(RecordingNotifier {
            sent: notification_tx,
        }));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let item = song("notification-runtime");
    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: item.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&item.id),
    }))?;
    let generation = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(generation) = state_rx
                .recv()
                .await
                .and_then(|state| state.current_attempt_generation())
            {
                return generation;
            }
        }
    })
    .await?;
    event_tx.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Playing,
    }))?;

    let notification = tokio::time::timeout(Duration::from_secs(1), notification_rx.recv())
        .await?
        .ok_or("notification task closed")?;
    assert_eq!(notification.generation(), generation);
    assert_eq!(notification.title(), item.title);
    event_tx.send(RuntimeEvent::Quit)?;
    runtime_task.await??;
    Ok(())
}

#[tokio::test]
async fn notification_cannot_delay_runtime_terminal_cleanup_past_deadline() -> TestResult {
    let notifier = Arc::new(NonCooperativeRuntimeNotifier::default());
    let terminal_calls = Arc::new(Mutex::new(Vec::new()));
    let services = RuntimeServices::new(Arc::new(StartupStorage { checkpoint: None }))
        .with_player(Arc::new(AcceptingPlayer))
        .with_notifier(notifier.clone())
        .with_terminal(Arc::new(RecordingTerminal {
            calls: Arc::clone(&terminal_calls),
        }))
        .with_shutdown_timeout(Duration::from_millis(25));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let runtime_task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));
    let item = song("notification-deadline");
    event_tx.send(RuntimeEvent::Action(Action::EnqueueMedia {
        item: item.clone(),
    }))?;
    event_tx.send(RuntimeEvent::Action(Action::PlayQueueItem {
        id: stable_queue_item_id(&item.id),
    }))?;
    let generation = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(generation) = state_rx
                .recv()
                .await
                .and_then(|state| state.current_attempt_generation())
            {
                return generation;
            }
        }
    })
    .await?;
    event_tx.send(RuntimeEvent::Action(Action::PlayerStatusChanged {
        generation,
        status: PlaybackStatus::Playing,
    }))?;
    notifier.started.notified().await;

    event_tx.send(RuntimeEvent::Quit)?;
    tokio::time::timeout(Duration::from_millis(250), runtime_task).await???;
    let calls = terminal_calls
        .lock()
        .map_err(|_| "terminal calls poisoned")?;
    assert!(calls.ends_with(&["disable_raw", "disable_mouse", "show_cursor", "leave_alt"]));
    Ok(())
}

#[test]
fn terminal_guard_restores_in_reverse_order_and_is_idempotent() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut guard = TerminalGuard::acquire(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))?;

    assert_eq!(
        calls
            .lock()
            .map_err(|_| "terminal calls poisoned")?
            .as_slice(),
        ["enter_alt", "hide_cursor", "enable_mouse", "enable_raw"]
    );

    guard.restore()?;
    guard.restore()?;
    drop(guard);

    assert_eq!(
        calls
            .lock()
            .map_err(|_| "terminal calls poisoned")?
            .as_slice(),
        [
            "enter_alt",
            "hide_cursor",
            "enable_mouse",
            "enable_raw",
            "disable_raw",
            "disable_mouse",
            "show_cursor",
            "leave_alt",
        ]
    );
    Ok(())
}

#[tokio::test]
#[serial_test::serial(runtime_panic_hook)]
async fn normal_quit_checkpoints_shuts_down_player_and_restores_terminal() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let services = cleanup_services(Arc::clone(&calls));
    Runtime::new(Config::default(), services)
        .run(
            OneEvent::new(RuntimeEvent::Quit),
            RecordingRenderer {
                states: Arc::new(Mutex::new(Vec::new())),
            },
        )
        .await?;

    assert_clean_shutdown(&calls)?;
    Ok(())
}

#[tokio::test]
#[serial_test::serial(runtime_panic_hook)]
async fn signal_checkpoints_shuts_down_player_and_restores_terminal() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let services = cleanup_services(Arc::clone(&calls));
    Runtime::new(Config::default(), services)
        .run(
            OneEvent::new(RuntimeEvent::Signal),
            RecordingRenderer {
                states: Arc::new(Mutex::new(Vec::new())),
            },
        )
        .await?;

    assert_clean_shutdown(&calls)?;
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn hung_storage_save_cannot_block_quit_cleanup_deadline() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (save_started_tx, mut save_started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(HungSaveStorage {
        save_started: save_started_tx,
    }))
    .with_player(Arc::new(CleanupPlayer {
        calls: Arc::clone(&calls),
    }))
    .with_terminal(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))
    .with_shutdown_timeout(Duration::from_millis(25));
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        OneEvent::new(RuntimeEvent::Quit),
        RecordingRenderer {
            states: Arc::new(Mutex::new(Vec::new())),
        },
    ));

    save_started_rx
        .recv()
        .await
        .ok_or("final session save did not start")?;
    tokio::time::advance(Duration::from_millis(25)).await;
    let result = tokio::time::timeout(Duration::from_millis(1), task)
        .await
        .map_err(|_| "runtime exceeded the cleanup deadline while storage save was pending")?;
    result??;
    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
    assert!(calls.contains(&"player_shutdown"));
    assert!(calls.contains(&"disable_raw"));
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn hung_storage_load_cannot_block_signal_cleanup_deadline() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (load_started_tx, mut load_started_rx) = mpsc::unbounded_channel();
    let (shutdown_started_tx, mut shutdown_started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(HungLoadStorage {
        load_started: load_started_tx,
    }))
    .with_player(Arc::new(NotifyingCleanupPlayer {
        calls: Arc::clone(&calls),
        shutdown_started: shutdown_started_tx,
    }))
    .with_terminal(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))
    .with_shutdown_timeout(Duration::from_millis(25));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        RecordingRenderer {
            states: Arc::new(Mutex::new(Vec::new())),
        },
    ));

    event_tx.send(RuntimeEvent::Action(Action::HistoryRequested))?;
    load_started_rx
        .recv()
        .await
        .ok_or("history load did not start")?;
    event_tx.send(RuntimeEvent::Signal)?;
    shutdown_started_rx
        .recv()
        .await
        .ok_or("player shutdown was not attempted")?;
    tokio::time::timeout(Duration::from_millis(1), task).await???;

    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
    assert!(calls.contains(&"player_shutdown"));
    assert!(calls.contains(&"disable_raw"));
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn hung_storage_load_cannot_block_injected_panic_cleanup() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (load_started_tx, mut load_started_rx) = mpsc::unbounded_channel();
    let (shutdown_started_tx, mut shutdown_started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(HungLoadStorage {
        load_started: load_started_tx,
    }))
    .with_player(Arc::new(NotifyingCleanupPlayer {
        calls: Arc::clone(&calls),
        shutdown_started: shutdown_started_tx,
    }))
    .with_terminal(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))
    .with_shutdown_timeout(Duration::from_millis(25));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        RecordingRenderer {
            states: Arc::new(Mutex::new(Vec::new())),
        },
    ));

    event_tx.send(RuntimeEvent::Action(Action::HistoryRequested))?;
    load_started_rx
        .recv()
        .await
        .ok_or("history load did not start")?;
    event_tx.send(RuntimeEvent::Panic)?;
    shutdown_started_rx
        .recv()
        .await
        .ok_or("player shutdown was not attempted")?;
    let result = tokio::time::timeout(Duration::from_millis(1), task).await?;
    let Err(join_error) = result else {
        return Err("injected panic did not resume after hung storage cleanup".into());
    };
    assert!(join_error.is_panic());

    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
    assert!(calls.contains(&"player_shutdown"));
    assert!(calls.contains(&"disable_raw"));
    Ok(())
}

#[tokio::test]
#[serial_test::serial(runtime_panic_hook)]
async fn injected_panic_cleans_up_then_resumes_unwind() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let services = cleanup_services(Arc::clone(&calls));
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        OneEvent::new(RuntimeEvent::Panic),
        RecordingRenderer {
            states: Arc::new(Mutex::new(Vec::new())),
        },
    ));

    let Err(join_error) = task.await else {
        return Err("injected panic did not resume after cleanup".into());
    };
    assert!(join_error.is_panic());
    assert_panic_cleanup(&calls)?;
    Ok(())
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(runtime_panic_hook)]
async fn hung_player_shutdown_hits_bounded_abort_path_before_terminal_restore() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (shutdown_started_tx, mut shutdown_started_rx) = mpsc::unbounded_channel();
    let services = RuntimeServices::new(Arc::new(CleanupStorage {
        calls: Arc::clone(&calls),
    }))
    .with_player(Arc::new(HungShutdownPlayer {
        calls: Arc::clone(&calls),
        shutdown_started: shutdown_started_tx,
    }))
    .with_terminal(Arc::new(RecordingTerminal {
        calls: Arc::clone(&calls),
    }))
    .with_shutdown_timeout(Duration::from_millis(25));
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        OneEvent::new(RuntimeEvent::Quit),
        RecordingRenderer {
            states: Arc::new(Mutex::new(Vec::new())),
        },
    ));

    shutdown_started_rx
        .recv()
        .await
        .ok_or("player shutdown was not attempted")?;
    tokio::time::advance(Duration::from_millis(25)).await;
    task.await??;

    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
    let checkpoint = calls
        .iter()
        .position(|call| *call == "checkpoint")
        .ok_or("checkpoint missing")?;
    let shutdown = calls
        .iter()
        .position(|call| *call == "player_shutdown")
        .ok_or("player shutdown missing")?;
    let abort = calls
        .iter()
        .position(|call| *call == "player_abort")
        .ok_or("hung player was not aborted")?;
    let restore = calls
        .iter()
        .position(|call| *call == "disable_raw")
        .ok_or("terminal was not restored")?;
    assert!(checkpoint < shutdown);
    assert!(shutdown < abort);
    assert!(abort < restore, "cleanup order changed: {calls:?}");
    Ok(())
}

#[tokio::test]
#[serial_test::serial(runtime_panic_hook)]
async fn panic_hook_restores_terminal_before_delegating_to_previous_hook() -> TestResult {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let hook_reset = RecordingPanicHook::install(Arc::clone(&calls));
    let services = cleanup_services(Arc::clone(&calls));
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        OneEvent::new(RuntimeEvent::Panic),
        RecordingRenderer {
            states: Arc::new(Mutex::new(Vec::new())),
        },
    ));

    let Err(join_error) = task.await else {
        return Err("panic did not resume after cleanup".into());
    };
    assert!(join_error.is_panic());

    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?.clone();
    drop(hook_reset);
    let acquired = calls
        .iter()
        .position(|call| *call == "enable_raw")
        .ok_or("terminal guard was not acquired")?;
    let restored = calls
        .iter()
        .position(|call| *call == "disable_raw")
        .ok_or("terminal was not restored")?;
    let delegated = calls
        .iter()
        .position(|call| *call == "panic_report")
        .ok_or("previous panic hook was not called")?;
    assert!(acquired < restored);
    assert!(restored < delegated);
    Ok(())
}

type BoxedPanicHook = Box<dyn for<'a> Fn(&PanicHookInfo<'a>) + Send + Sync + 'static>;

struct RecordingPanicHook {
    previous: Option<BoxedPanicHook>,
}

impl RecordingPanicHook {
    fn install(calls: Arc<Mutex<Vec<&'static str>>>) -> Self {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |_| {
            if let Ok(mut calls) = calls.lock() {
                calls.push("panic_report");
            }
        }));
        Self {
            previous: Some(previous),
        }
    }
}

impl Drop for RecordingPanicHook {
    fn drop(&mut self) {
        let _ = panic::take_hook();
        if let Some(previous) = self.previous.take() {
            panic::set_hook(previous);
        }
    }
}

fn cleanup_services(calls: Arc<Mutex<Vec<&'static str>>>) -> RuntimeServices {
    RuntimeServices::new(Arc::new(CleanupStorage {
        calls: Arc::clone(&calls),
    }))
    .with_player(Arc::new(CleanupPlayer {
        calls: Arc::clone(&calls),
    }))
    .with_terminal(Arc::new(RecordingTerminal { calls }))
    .with_shutdown_timeout(Duration::from_millis(100))
}

fn assert_clean_shutdown(calls: &Arc<Mutex<Vec<&'static str>>>) -> TestResult {
    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
    let checkpoint = calls
        .iter()
        .position(|call| *call == "checkpoint")
        .ok_or("final checkpoint was not saved")?;
    let player = calls
        .iter()
        .position(|call| *call == "player_shutdown")
        .ok_or("player was not shut down")?;
    let disable_raw = calls
        .iter()
        .position(|call| *call == "disable_raw")
        .ok_or("terminal raw mode was not restored")?;
    assert!(checkpoint < player);
    assert!(player < disable_raw);
    assert_eq!(
        &calls[disable_raw..],
        ["disable_raw", "disable_mouse", "show_cursor", "leave_alt"]
    );
    assert!(!calls.contains(&"player_abort"));
    Ok(())
}

fn assert_panic_cleanup(calls: &Arc<Mutex<Vec<&'static str>>>) -> TestResult {
    let calls = calls.lock().map_err(|_| "cleanup calls poisoned")?;
    let checkpoint = calls
        .iter()
        .position(|call| *call == "checkpoint")
        .ok_or("final checkpoint was not saved")?;
    let player = calls
        .iter()
        .position(|call| *call == "player_shutdown")
        .ok_or("player was not shut down")?;
    assert!(checkpoint < player);
    assert_eq!(
        calls
            .windows(4)
            .filter(|window| {
                *window == ["disable_raw", "disable_mouse", "show_cursor", "leave_alt"]
            })
            .count(),
        1
    );
    assert!(!calls.contains(&"player_abort"));
    Ok(())
}

struct StartupStorage {
    checkpoint: Option<SessionCheckpoint>,
}

#[async_trait]
impl RuntimeStorage for StartupStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(self.checkpoint.clone())
    }
}

struct RecordingStartup {
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl RecordingStartup {
    fn record(&self, call: &'static str) -> anyhow::Result<()> {
        self.calls
            .lock()
            .map_err(|_| anyhow::anyhow!("startup calls poisoned"))?
            .push(call);
        Ok(())
    }
}

#[async_trait]
impl StartupFactory for RecordingStartup {
    fn resolve_paths(&self) -> anyhow::Result<AppPaths> {
        self.record("paths")?;
        Ok(AppPaths::from_roots(
            PathBuf::from("/config"),
            PathBuf::from("/data"),
            PathBuf::from("/cache"),
        ))
    }

    fn initialize_logging(&self, _paths: &AppPaths) -> anyhow::Result<Box<dyn Send>> {
        self.record("logging")?;
        Ok(Box::new(()))
    }

    fn load_config(&self, _paths: &AppPaths) -> anyhow::Result<Config> {
        self.record("config")?;
        Ok(Config::default())
    }

    async fn migrate_storage(&self, _paths: &AppPaths) -> anyhow::Result<Arc<dyn RuntimeStorage>> {
        self.record("storage")?;
        Ok(Arc::new(StartupStorage { checkpoint: None }))
    }

    async fn load_credentials(&self) -> anyhow::Result<Option<SecretString>> {
        self.record("credentials")?;
        Ok(Some(SecretString::from("opaque-cookie".to_owned())))
    }

    async fn construct_provider(
        &self,
        credentials: Option<SecretString>,
    ) -> anyhow::Result<Arc<dyn MusicProvider>> {
        if credentials.is_none() {
            return Err(anyhow::anyhow!("credentials were not forwarded"));
        }
        self.record("provider")?;
        let (started, _started_rx) = mpsc::unbounded_channel();
        Ok(Arc::new(ConcurrentProvider {
            started,
            release: Arc::new(Semaphore::new(0)),
        }))
    }

    async fn check_dependencies(&self) -> anyhow::Result<ytermusic::diagnostics::DoctorReport> {
        self.record("dependencies")?;
        Ok(ytermusic::diagnostics::DoctorReport::new(Vec::new()))
    }

    async fn enter_tui(
        &self,
        _paths: AppPaths,
        _config: Config,
        _storage: Arc<dyn RuntimeStorage>,
        _provider: Arc<dyn MusicProvider>,
        _dependencies: ytermusic::diagnostics::DoctorReport,
    ) -> anyhow::Result<()> {
        self.record("tui")
    }
}

struct OneEvent {
    event: Option<RuntimeEvent>,
}

struct OneThenPending {
    event: Option<RuntimeEvent>,
}

impl OneThenPending {
    const fn new(event: RuntimeEvent) -> Self {
        Self { event: Some(event) }
    }
}

#[async_trait]
impl EventSource for OneThenPending {
    async fn next_event(&mut self) -> Option<RuntimeEvent> {
        match self.event.take() {
            Some(event) => Some(event),
            None => future::pending().await,
        }
    }
}

impl OneEvent {
    const fn new(event: RuntimeEvent) -> Self {
        Self { event: Some(event) }
    }
}

#[async_trait]
impl EventSource for OneEvent {
    async fn next_event(&mut self) -> Option<RuntimeEvent> {
        self.event.take()
    }
}

struct ChannelEvents {
    receiver: mpsc::UnboundedReceiver<RuntimeEvent>,
}

#[async_trait]
impl EventSource for ChannelEvents {
    async fn next_event(&mut self) -> Option<RuntimeEvent> {
        self.receiver.recv().await
    }
}

struct AcknowledgedEvents {
    receiver: mpsc::UnboundedReceiver<(RuntimeEvent, oneshot::Sender<()>)>,
}

#[async_trait]
impl EventSource for AcknowledgedEvents {
    async fn next_event(&mut self) -> Option<RuntimeEvent> {
        let (event, acknowledged) = self.receiver.recv().await?;
        let _ = acknowledged.send(());
        Some(event)
    }
}

struct RecordingRenderer {
    states: Arc<Mutex<Vec<AppState>>>,
}

struct StateChannelRenderer {
    states: mpsc::UnboundedSender<AppState>,
}

struct ModelChannelRenderer {
    snapshots: mpsc::UnboundedSender<(AppState, RenderModel)>,
}

impl Renderer for StateChannelRenderer {
    fn render(&mut self, state: &AppState) -> io::Result<()> {
        self.states
            .send(state.clone())
            .map_err(|_| io::Error::other("runtime state receiver closed"))
    }
}

impl Renderer for ModelChannelRenderer {
    fn render(&mut self, state: &AppState) -> io::Result<()> {
        self.render_with_model(state, &RenderModel::default())
    }

    fn render_with_model(&mut self, state: &AppState, model: &RenderModel) -> io::Result<()> {
        self.snapshots
            .send((state.clone(), model.clone()))
            .map_err(|_| io::Error::other("runtime model receiver closed"))
    }
}

struct ConcurrentProvider {
    started: mpsc::UnboundedSender<&'static str>,
    release: Arc<Semaphore>,
}

struct CancellingSearchProvider {
    started: mpsc::UnboundedSender<()>,
    cancelled: mpsc::UnboundedSender<()>,
}

struct SupersedingProvider {
    active: Arc<AtomicUsize>,
    chart_started: Option<mpsc::UnboundedSender<usize>>,
    podcast_started: Option<mpsc::UnboundedSender<usize>>,
}

struct ActiveBoundary {
    active: Arc<AtomicUsize>,
}

impl Drop for ActiveBoundary {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

struct PaginationProvider {
    calls: mpsc::UnboundedSender<SearchCall>,
}

struct DropNotice {
    cancelled: mpsc::UnboundedSender<()>,
}

impl Drop for DropNotice {
    fn drop(&mut self) {
        let _ = self.cancelled.send(());
    }
}

#[async_trait]
impl MusicProvider for CancellingSearchProvider {
    async fn search(&self, query: &str, _filter: SearchFilter) -> ProviderResult<Page<SearchItem>> {
        if query == "first" {
            let _ = self.started.send(());
            let _notice = DropNotice {
                cancelled: self.cancelled.clone(),
            };
            future::pending::<()>().await;
            unreachable!("pending provider future only exits by cancellation");
        }
        Ok(Page {
            items: vec![SearchItem::Playable(song("second-result"))],
            continuation: None,
            stale: false,
        })
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        Err(unavailable(ProviderOperation::Charts))
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Playlist))
    }

    async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
        Err(unavailable(ProviderOperation::Podcast))
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Radio))
    }

    async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        Err(unavailable(ProviderOperation::Library))
    }

    fn authentication(&self) -> AuthenticationState {
        AuthenticationState::Unauthenticated
    }
}

#[async_trait]
impl MusicProvider for SupersedingProvider {
    async fn search(
        &self,
        _query: &str,
        _filter: SearchFilter,
    ) -> ProviderResult<Page<SearchItem>> {
        Ok(Page {
            items: vec![SearchItem::Podcast(BrowseItem {
                id: "bounded-show".to_owned(),
                title: "Bounded show".to_owned(),
                subtitle: None,
                artwork_url: None,
            })],
            continuation: None,
            stale: false,
        })
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        let Some(started) = &self.chart_started else {
            return Err(unavailable(ProviderOperation::Charts));
        };
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        started
            .send(active)
            .map_err(|_| unavailable(ProviderOperation::Charts))?;
        let _active = ActiveBoundary {
            active: Arc::clone(&self.active),
        };
        future::pending().await
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Playlist))
    }

    async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
        let Some(started) = &self.podcast_started else {
            return Err(unavailable(ProviderOperation::Podcast));
        };
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        started
            .send(active)
            .map_err(|_| unavailable(ProviderOperation::Podcast))?;
        let _active = ActiveBoundary {
            active: Arc::clone(&self.active),
        };
        future::pending().await
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Radio))
    }

    async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        Err(unavailable(ProviderOperation::Library))
    }

    fn authentication(&self) -> AuthenticationState {
        AuthenticationState::Unauthenticated
    }
}

#[async_trait]
impl MusicProvider for PaginationProvider {
    async fn search(&self, query: &str, _filter: SearchFilter) -> ProviderResult<Page<SearchItem>> {
        self.calls
            .send(SearchCall::Initial(query.to_owned()))
            .map_err(|_| unavailable(ProviderOperation::Search))?;
        Ok(Page {
            items: vec![SearchItem::Playable(song("page-one"))],
            continuation: Some("next-page".to_owned()),
            stale: false,
        })
    }

    async fn search_more(
        &self,
        _query: &str,
        _filter: SearchFilter,
        continuation: &str,
    ) -> ProviderResult<Page<SearchItem>> {
        self.calls
            .send(SearchCall::More(continuation.to_owned()))
            .map_err(|_| unavailable(ProviderOperation::Search))?;
        Ok(Page {
            items: vec![SearchItem::Playable(song("page-two"))],
            continuation: None,
            stale: false,
        })
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        Err(unavailable(ProviderOperation::Charts))
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Playlist))
    }

    async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
        Err(unavailable(ProviderOperation::Podcast))
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Radio))
    }

    async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        Err(unavailable(ProviderOperation::Library))
    }

    fn authentication(&self) -> AuthenticationState {
        AuthenticationState::Unauthenticated
    }
}

#[async_trait]
impl MusicProvider for ConcurrentProvider {
    async fn search(
        &self,
        _query: &str,
        _filter: SearchFilter,
    ) -> ProviderResult<Page<SearchItem>> {
        let _ = self.started.send("search");
        let permit = self
            .release
            .acquire()
            .await
            .map_err(|_| unavailable(ProviderOperation::Search))?;
        permit.forget();
        Ok(Page {
            items: vec![SearchItem::Playable(song("search-result"))],
            continuation: None,
            stale: false,
        })
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        let _ = self.started.send("charts");
        let permit = self
            .release
            .acquire()
            .await
            .map_err(|_| unavailable(ProviderOperation::Charts))?;
        permit.forget();
        Ok(vec![ChartSection::new(
            "Concurrent chart",
            vec![song("chart-result")],
        )])
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Playlist))
    }

    async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
        Err(unavailable(ProviderOperation::Podcast))
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        Err(unavailable(ProviderOperation::Radio))
    }

    async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        Err(unavailable(ProviderOperation::Library))
    }

    fn authentication(&self) -> AuthenticationState {
        AuthenticationState::Unauthenticated
    }
}

const fn unavailable(operation: ProviderOperation) -> ProviderError {
    ProviderError::new(operation, ProviderErrorKind::Unavailable)
}

struct ThreadRecordingStorage {
    called: std_mpsc::Sender<thread::ThreadId>,
    favorite_events: Option<std_mpsc::Sender<&'static str>>,
}

struct BlockingSyncChartStorage {
    gate: Arc<(Mutex<bool>, Condvar)>,
    calls: Arc<AtomicUsize>,
}

struct RecordingRuntimeStorage {
    saved: mpsc::UnboundedSender<(SessionCheckpoint, i64)>,
}

#[derive(Debug)]
enum DrainStorageEvent {
    History(String),
    Session,
}

struct DrainRecordingStorage {
    events: mpsc::UnboundedSender<DrainStorageEvent>,
}

struct SupersedingChartCacheStorage {
    active: Arc<AtomicUsize>,
    started: mpsc::UnboundedSender<usize>,
}

#[async_trait]
impl RuntimeStorage for DrainRecordingStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        self.events
            .send(DrainStorageEvent::Session)
            .map_err(|_| RuntimeStorageError)
    }

    async fn record_history(
        &self,
        item: MediaItem,
        _played_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        self.events
            .send(DrainStorageEvent::History(item.id.video_id))
            .map_err(|_| RuntimeStorageError)
    }
}

#[async_trait]
impl RuntimeStorage for SupersedingChartCacheStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Ok(())
    }

    async fn load_chart_cache(
        &self,
        _key: &str,
    ) -> Result<Option<MetadataCacheEntry>, RuntimeStorageError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.started.send(active).map_err(|_| RuntimeStorageError)?;
        let _active = ActiveBoundary {
            active: Arc::clone(&self.active),
        };
        future::pending().await
    }
}

#[async_trait]
impl RuntimeStorage for RecordingRuntimeStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        checkpoint: SessionCheckpoint,
        updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        self.saved
            .send((checkpoint, updated_at))
            .map_err(|_| RuntimeStorageError)
    }
}

struct FixedClock(i64);

impl RuntimeClock for FixedClock {
    fn now_millis(&self) -> i64 {
        self.0
    }
}

struct FavoriteRuntimeStorage {
    entries: Mutex<Vec<FavoriteEntry>>,
    outcome: FavoriteInsertOutcome,
    failure: Option<FavoriteStorageFailure>,
    events: mpsc::UnboundedSender<String>,
}

struct FailingMouseRenderer {
    rendered: mpsc::UnboundedSender<RenderModel>,
    interactions: InteractionStore,
    render_count: usize,
}

#[derive(Clone)]
struct KeyboardRenderTrace {
    view: NavigationItem,
    artwork_surface: ArtworkSurface,
    requested_artwork: Option<ArtworkUrl>,
    history_loading: bool,
}

struct KeyboardTraceRenderer {
    rendered: mpsc::UnboundedSender<KeyboardRenderTrace>,
    fail_on_favorites: bool,
}

impl Renderer for KeyboardTraceRenderer {
    fn render(&mut self, state: &AppState) -> io::Result<()> {
        self.render_with_model(state, &RenderModel::default())
    }

    fn render_with_model(&mut self, state: &AppState, model: &RenderModel) -> io::Result<()> {
        self.rendered
            .send(KeyboardRenderTrace {
                view: model.view,
                artwork_surface: state.artwork_surface(),
                requested_artwork: state.artwork().requested_url().cloned(),
                history_loading: state.history().loading(),
            })
            .map_err(|_| io::Error::other("keyboard render receiver closed"))?;
        if self.fail_on_favorites && model.view == NavigationItem::Favorites {
            return Err(io::Error::other("injected keyboard render failure"));
        }
        Ok(())
    }
}

struct BlockingFavoriteLoadStorage {
    favorite_load_started: mpsc::UnboundedSender<()>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl RuntimeStorage for BlockingFavoriteLoadStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn load_history(&self, _limit: usize) -> Result<Vec<HistoryEntry>, RuntimeStorageError> {
        Ok(Vec::new())
    }

    async fn load_favorites(&self) -> Result<Vec<FavoriteEntry>, RuntimeStorageError> {
        self.favorite_load_started
            .send(())
            .map_err(|_| RuntimeStorageError)?;
        self.release
            .acquire()
            .await
            .map_err(|_| RuntimeStorageError)?
            .forget();
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn keyboard_navigation_renders_only_final_favorites_artwork_owner() -> TestResult {
    let search_artwork = artwork_url("https://images.example.test/key-search-owner.png")?;
    let (load_started_tx, mut load_started_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let storage = Arc::new(BlockingFavoriteLoadStorage {
        favorite_load_started: load_started_tx,
        release: Arc::clone(&release),
    });
    let services = RuntimeServices::new(storage)
        .with_initial_action(Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Search,
        })
        .with_initial_action(Action::ArtworkRequested {
            url: search_artwork.clone(),
        });
    let (rendered_tx, mut rendered_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        KeyboardTraceRenderer {
            rendered: rendered_tx,
            fail_on_favorites: false,
        },
    ));
    rendered_rx.recv().await.ok_or("initial render missing")?;
    event_tx.send(RuntimeEvent::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )))?;
    rendered_rx
        .recv()
        .await
        .ok_or("navigation focus render missing")?;

    for (view, surface) in [
        (NavigationItem::Settings, ArtworkSurface::Settings),
        (NavigationItem::History, ArtworkSurface::History),
    ] {
        event_tx.send(RuntimeEvent::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )))?;
        loop {
            let trace = tokio::time::timeout(Duration::from_secs(1), rendered_rx.recv())
                .await
                .map_err(|_| io::Error::other(format!("timed out waiting for {view:?}")))?
                .ok_or("navigation render missing")?;
            if trace.view == view
                && trace.artwork_surface == surface
                && (view != NavigationItem::History || !trace.history_loading)
            {
                break;
            }
        }
    }

    event_tx.send(RuntimeEvent::Key(KeyEvent::new(
        KeyCode::Char('h'),
        KeyModifiers::NONE,
    )))?;
    tokio::time::timeout(Duration::from_secs(1), load_started_rx.recv())
        .await?
        .ok_or("favorites load did not start")?;
    let mut favorites_frames = Vec::new();
    while let Ok(trace) = rendered_rx.try_recv() {
        if trace.view == NavigationItem::Favorites {
            favorites_frames.push(trace);
        }
    }

    assert_eq!(favorites_frames.len(), 1, "favorites render trace");
    let frame = &favorites_frames[0];
    assert_eq!(frame.artwork_surface, ArtworkSurface::Favorites);
    assert_ne!(frame.requested_artwork.as_ref(), Some(&search_artwork));

    release.add_permits(1);
    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn keyboard_render_failure_prevents_generated_effect_dispatch() -> TestResult {
    let (load_started_tx, mut load_started_rx) = mpsc::unbounded_channel();
    let storage = Arc::new(BlockingFavoriteLoadStorage {
        favorite_load_started: load_started_tx,
        release: Arc::new(Semaphore::new(0)),
    });
    let (rendered_tx, mut rendered_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(
        Runtime::new(Config::default(), RuntimeServices::new(storage)).run(
            ChannelEvents { receiver: event_rx },
            KeyboardTraceRenderer {
                rendered: rendered_tx,
                fail_on_favorites: true,
            },
        ),
    );
    rendered_rx.recv().await.ok_or("initial render missing")?;
    event_tx.send(RuntimeEvent::Key(KeyEvent::new(
        KeyCode::BackTab,
        KeyModifiers::SHIFT,
    )))?;
    rendered_rx
        .recv()
        .await
        .ok_or("navigation focus render missing")?;

    for expected in [NavigationItem::Settings, NavigationItem::History] {
        event_tx.send(RuntimeEvent::Key(KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )))?;
        loop {
            let trace = tokio::time::timeout(Duration::from_secs(1), rendered_rx.recv())
                .await?
                .ok_or("navigation render missing")?;
            if trace.view == expected
                && (expected != NavigationItem::History || !trace.history_loading)
            {
                break;
            }
        }
    }
    event_tx.send(RuntimeEvent::Key(KeyEvent::new(
        KeyCode::Char('h'),
        KeyModifiers::NONE,
    )))?;

    assert!(matches!(task.await?, Err(RuntimeError::Render(_))));
    assert!(
        load_started_rx.try_recv().is_err(),
        "key-generated effect ran before failed render"
    );
    Ok(())
}

impl Renderer for FailingMouseRenderer {
    fn interaction_snapshot(&self) -> Option<InteractionSnapshot> {
        self.interactions.latest().cloned()
    }

    fn render(&mut self, state: &AppState) -> io::Result<()> {
        self.render_with_model(state, &RenderModel::default())
    }

    fn render_with_model(&mut self, _state: &AppState, model: &RenderModel) -> io::Result<()> {
        self.render_count = self.render_count.saturating_add(1);
        self.rendered
            .send(model.clone())
            .map_err(|_| io::Error::other("render receiver closed"))?;
        if self.render_count > 1 {
            return Err(io::Error::other("injected mouse render failure"));
        }
        let mut map = self
            .interactions
            .begin_frame()
            .ok_or_else(|| io::Error::other("interaction revision exhausted"))?;
        assert!(map.push(
            Rect::new(4, 7, 1, 1),
            HitTarget::Navigation(NavigationItem::Favorites),
        ));
        assert!(self.interactions.publish(map));
        Ok(())
    }
}

#[tokio::test]
async fn mouse_render_failure_prevents_generated_effect_dispatch() -> TestResult {
    let (storage_events, mut recorded) = mpsc::unbounded_channel();
    let storage = Arc::new(FavoriteRuntimeStorage {
        entries: Mutex::new(Vec::new()),
        outcome: FavoriteInsertOutcome::Added,
        failure: None,
        events: storage_events,
    });
    let (rendered_tx, mut rendered_rx) = mpsc::unbounded_channel();
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(
        Runtime::new(Config::default(), RuntimeServices::new(storage)).run(
            ChannelEvents { receiver: event_rx },
            FailingMouseRenderer {
                rendered: rendered_tx,
                interactions: InteractionStore::default(),
                render_count: 0,
            },
        ),
    );
    rendered_rx.recv().await.ok_or("initial render missing")?;

    event_tx.send(RuntimeEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row: 7,
        modifiers: KeyModifiers::NONE,
    }))?;
    let result = task.await?;

    assert!(matches!(result, Err(RuntimeError::Render(_))));
    assert!(
        recorded.try_recv().is_err(),
        "effect ran before failed render"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FavoriteStorageFailure {
    Load,
    Add,
    Remove,
}

#[tokio::test]
async fn favorites_commands_use_ordered_storage_and_runtime_clock() -> TestResult {
    let item = song("ordered-favorite");
    let (storage_events, mut recorded) = mpsc::unbounded_channel();
    let storage = Arc::new(FavoriteRuntimeStorage {
        entries: Mutex::new(Vec::new()),
        outcome: FavoriteInsertOutcome::Added,
        failure: None,
        events: storage_events,
    });
    let services = RuntimeServices::new(storage)
        .with_clock(Arc::new(FixedClock(1_700_000_123)))
        .with_initial_action(Action::FavoritesRequested)
        .with_initial_action(Action::FavoritesCompleted {
            generation: Generation::new(1),
            result: Ok(Vec::new()),
        })
        .with_initial_action(Action::FavoriteToggleRequested { item: item.clone() });
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut states) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    loop {
        let state = tokio::time::timeout(Duration::from_secs(1), states.recv())
            .await?
            .ok_or("state stream closed")?;
        if state.favorites().entries().len() == 1 && state.favorites().pending_mutation().is_none()
        {
            break;
        }
    }
    event_tx.send(RuntimeEvent::Action(Action::FavoriteToggleRequested {
        item: item.clone(),
    }))?;
    loop {
        let state = tokio::time::timeout(Duration::from_secs(1), states.recv())
            .await?
            .ok_or("state stream closed")?;
        if state.favorites().entries().is_empty() && state.favorites().pending_mutation().is_none()
        {
            break;
        }
    }
    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;

    let mut events = Vec::new();
    while let Ok(event) = recorded.try_recv() {
        events.push(event);
    }
    assert_eq!(events, ["load", "add:1700000123", "load", "remove", "load"]);
    Ok(())
}

#[tokio::test]
async fn favorites_full_is_safe_category_completion() -> TestResult {
    let item = song("full-favorite");
    let (storage_events, _recorded) = mpsc::unbounded_channel();
    let storage = Arc::new(FavoriteRuntimeStorage {
        entries: Mutex::new(Vec::new()),
        outcome: FavoriteInsertOutcome::Full,
        failure: None,
        events: storage_events,
    });
    let services = RuntimeServices::new(storage)
        .with_initial_action(Action::FavoritesRequested)
        .with_initial_action(Action::FavoritesCompleted {
            generation: Generation::new(1),
            result: Ok(Vec::new()),
        })
        .with_initial_action(Action::FavoriteToggleRequested { item });
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut states) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    let error = loop {
        let state = tokio::time::timeout(Duration::from_secs(1), states.recv())
            .await?
            .ok_or("state stream closed")?;
        if let Some(error) = state.favorites().error() {
            break error.clone();
        }
    };
    assert_eq!(error.category(), AppErrorCategory::Favorites);
    assert!(!error.message().contains("storage"));
    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn favorites_already_present_reloads_the_canonical_entry() -> TestResult {
    let item = song("already-present-favorite");
    let canonical = FavoriteEntry {
        id: 41,
        item: item.clone(),
        favorited_at: 123,
    };
    let (storage_events, _recorded) = mpsc::unbounded_channel();
    let storage = Arc::new(FavoriteRuntimeStorage {
        entries: Mutex::new(vec![canonical.clone()]),
        outcome: FavoriteInsertOutcome::AlreadyPresent,
        failure: None,
        events: storage_events,
    });
    let services = RuntimeServices::new(storage)
        .with_initial_action(Action::FavoritesRequested)
        .with_initial_action(Action::FavoritesCompleted {
            generation: Generation::new(1),
            result: Ok(Vec::new()),
        })
        .with_initial_action(Action::FavoriteToggleRequested { item });
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (state_tx, mut states) = mpsc::unbounded_channel();
    let task = tokio::spawn(Runtime::new(Config::default(), services).run(
        ChannelEvents { receiver: event_rx },
        StateChannelRenderer { states: state_tx },
    ));

    loop {
        let state = tokio::time::timeout(Duration::from_secs(1), states.recv())
            .await?
            .ok_or("state stream closed")?;
        if state.favorites().entries() == [canonical.clone()]
            && state.favorites().pending_mutation().is_none()
        {
            assert!(state.favorites().error().is_none());
            break;
        }
    }
    event_tx.send(RuntimeEvent::Quit)?;
    task.await??;
    Ok(())
}

#[tokio::test]
async fn favorites_storage_failures_are_safe_and_preserve_canonical_entries() -> TestResult {
    for failure in [
        FavoriteStorageFailure::Load,
        FavoriteStorageFailure::Add,
        FavoriteStorageFailure::Remove,
    ] {
        let canonical_item = song("failure-canonical");
        let canonical = FavoriteEntry {
            id: 7,
            item: canonical_item.clone(),
            favorited_at: 10,
        };
        let (storage_events, _recorded) = mpsc::unbounded_channel();
        let storage_entries = if failure == FavoriteStorageFailure::Load {
            Vec::new()
        } else {
            vec![canonical.clone()]
        };
        let storage = Arc::new(FavoriteRuntimeStorage {
            entries: Mutex::new(storage_entries),
            outcome: FavoriteInsertOutcome::Added,
            failure: Some(failure),
            events: storage_events,
        });
        let mut services =
            RuntimeServices::new(storage).with_initial_action(Action::FavoritesRequested);
        if failure != FavoriteStorageFailure::Load {
            services = services.with_initial_action(Action::FavoritesCompleted {
                generation: Generation::new(1),
                result: Ok(vec![canonical.clone()]),
            });
            let item = if failure == FavoriteStorageFailure::Remove {
                canonical_item
            } else {
                song("failed-add")
            };
            services = services.with_initial_action(Action::FavoriteToggleRequested { item });
        }
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (state_tx, mut states) = mpsc::unbounded_channel();
        let task = tokio::spawn(Runtime::new(Config::default(), services).run(
            ChannelEvents { receiver: event_rx },
            StateChannelRenderer { states: state_tx },
        ));

        let state = loop {
            let state = tokio::time::timeout(Duration::from_secs(1), states.recv())
                .await?
                .ok_or("state stream closed")?;
            if state.favorites().error().is_some() {
                break state;
            }
        };
        let error = state.favorites().error().ok_or("favorites error missing")?;
        assert_eq!(error.category(), AppErrorCategory::Favorites);
        assert!(!error.message().contains("storage"));
        assert!(state.favorites().pending_mutation().is_none());
        if failure != FavoriteStorageFailure::Load {
            assert_eq!(state.favorites().entries(), &[canonical]);
        }
        event_tx.send(RuntimeEvent::Quit)?;
        task.await??;
    }
    Ok(())
}

#[async_trait]
impl RuntimeStorage for FavoriteRuntimeStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn load_favorites(&self) -> Result<Vec<FavoriteEntry>, RuntimeStorageError> {
        self.events
            .send("load".to_owned())
            .map_err(|_| RuntimeStorageError)?;
        if self.failure == Some(FavoriteStorageFailure::Load) {
            return Err(RuntimeStorageError);
        }
        self.entries
            .lock()
            .map_err(|_| RuntimeStorageError)
            .map(|entries| entries.clone())
    }

    async fn add_favorite(
        &self,
        item: MediaItem,
        favorited_at: i64,
    ) -> Result<FavoriteInsertOutcome, RuntimeStorageError> {
        self.events
            .send(format!("add:{favorited_at}"))
            .map_err(|_| RuntimeStorageError)?;
        if self.failure == Some(FavoriteStorageFailure::Add) {
            return Err(RuntimeStorageError);
        }
        if self.outcome == FavoriteInsertOutcome::Added {
            self.entries
                .lock()
                .map_err(|_| RuntimeStorageError)?
                .insert(
                    0,
                    FavoriteEntry {
                        id: 1,
                        item,
                        favorited_at,
                    },
                );
        }
        Ok(self.outcome)
    }

    async fn remove_favorite(&self, id: MediaId) -> Result<bool, RuntimeStorageError> {
        self.events
            .send("remove".to_owned())
            .map_err(|_| RuntimeStorageError)?;
        if self.failure == Some(FavoriteStorageFailure::Remove) {
            return Err(RuntimeStorageError);
        }
        let mut entries = self.entries.lock().map_err(|_| RuntimeStorageError)?;
        let before = entries.len();
        entries.retain(|entry| entry.item.id != id);
        Ok(entries.len() != before)
    }
}

struct OrderedPodcastStorage {
    checkpoint: SessionCheckpoint,
    progress: Mutex<Option<PodcastProgress>>,
    events: mpsc::UnboundedSender<PodcastStorageEvent>,
}

struct RecordingPlayer {
    calls: mpsc::UnboundedSender<PlayerCall>,
}

struct AcceptingPlayer;

struct FailingPlayer;

struct BlockingVolumePlayer {
    started: mpsc::UnboundedSender<()>,
    release: Arc<Semaphore>,
}

struct SaturatedCleanupPlayer {
    calls: Arc<Mutex<Vec<&'static str>>>,
    started: mpsc::UnboundedSender<()>,
    release: Arc<Semaphore>,
}

struct SaturatingFailPlayer {
    calls: AtomicUsize,
    started: mpsc::UnboundedSender<()>,
    release_failure: Arc<Semaphore>,
}

struct OnePlayerAction {
    action: Option<Action>,
}

struct ActionAfterEventTask {
    event_finished: Option<oneshot::Receiver<()>>,
    actions: VecDeque<Action>,
    action_accepted: Option<oneshot::Sender<()>>,
}

struct DropSignalledEvents {
    events: VecDeque<RuntimeEvent>,
    finished: Option<oneshot::Sender<()>>,
}

impl Drop for DropSignalledEvents {
    fn drop(&mut self) {
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(());
        }
    }
}

#[async_trait]
impl EventSource for DropSignalledEvents {
    async fn next_event(&mut self) -> Option<RuntimeEvent> {
        self.events.pop_front()
    }
}

#[async_trait]
impl RuntimePlayerActions for ActionAfterEventTask {
    async fn next_action(&mut self) -> Option<Action> {
        if let Some(action) = self.actions.pop_front() {
            if let Some(event_finished) = self.event_finished.take() {
                let _ = event_finished.await;
            }
            return Some(action);
        }
        if let Some(action_accepted) = self.action_accepted.take() {
            let _ = action_accepted.send(());
        }
        future::pending().await
    }
}

#[async_trait]
impl RuntimePlayerActions for OnePlayerAction {
    async fn next_action(&mut self) -> Option<Action> {
        self.action.take()
    }
}

struct RecordingTerminal {
    calls: Arc<Mutex<Vec<&'static str>>>,
}

struct RestorationNotifyingTerminal {
    terminal: RecordingTerminal,
    restored: mpsc::UnboundedSender<()>,
}

struct BlockingFirstRenderer {
    started: mpsc::UnboundedSender<()>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    first: bool,
}

impl Renderer for BlockingFirstRenderer {
    fn render(&mut self, _state: &AppState) -> io::Result<()> {
        if !self.first {
            return Ok(());
        }
        self.first = false;
        self.started
            .send(())
            .map_err(|_| io::Error::other("render-start receiver closed"))?;
        let (released, wake) = &*self.gate;
        let mut released = released
            .lock()
            .map_err(|_| io::Error::other("render gate poisoned"))?;
        while !*released {
            released = wake
                .wait(released)
                .map_err(|_| io::Error::other("render gate poisoned"))?;
        }
        Ok(())
    }
}

struct CleanupStorage {
    calls: Arc<Mutex<Vec<&'static str>>>,
}

struct HungSaveStorage {
    save_started: mpsc::UnboundedSender<()>,
}

struct HungLoadStorage {
    load_started: mpsc::UnboundedSender<()>,
}

struct HungHistoryStorage {
    started: mpsc::UnboundedSender<()>,
}

struct BlockingPodcastStorage {
    save_started: mpsc::UnboundedSender<()>,
    release: Arc<Semaphore>,
}

struct SaturatingPodcastStorage {
    save_started: mpsc::UnboundedSender<PodcastProgress>,
    saved: mpsc::UnboundedSender<PodcastProgress>,
    release: Arc<Semaphore>,
}

#[async_trait]
impl RuntimeStorage for BlockingPodcastStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Ok(())
    }

    async fn load_podcast_progress(
        &self,
        _video_id: String,
    ) -> Result<Option<PodcastProgress>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_podcast_progress(
        &self,
        _progress: PodcastProgress,
    ) -> Result<(), RuntimeStorageError> {
        self.save_started
            .send(())
            .map_err(|_| RuntimeStorageError)?;
        self.release
            .acquire()
            .await
            .map_err(|_| RuntimeStorageError)?
            .forget();
        Ok(())
    }
}

#[async_trait]
impl RuntimeStorage for SaturatingPodcastStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Ok(())
    }

    async fn load_podcast_progress(
        &self,
        _video_id: String,
    ) -> Result<Option<PodcastProgress>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_podcast_progress(
        &self,
        progress: PodcastProgress,
    ) -> Result<(), RuntimeStorageError> {
        self.save_started
            .send(progress.clone())
            .map_err(|_| RuntimeStorageError)?;
        self.release
            .acquire()
            .await
            .map_err(|_| RuntimeStorageError)?
            .forget();
        self.saved.send(progress).map_err(|_| RuntimeStorageError)
    }
}

#[async_trait]
impl RuntimeStorage for HungLoadStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Ok(())
    }

    async fn load_history(&self, _limit: usize) -> Result<Vec<HistoryEntry>, RuntimeStorageError> {
        self.load_started
            .send(())
            .map_err(|_| RuntimeStorageError)?;
        future::pending().await
    }
}

#[async_trait]
impl RuntimeStorage for HungHistoryStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Ok(())
    }

    async fn record_history(
        &self,
        _item: MediaItem,
        _played_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        self.started.send(()).map_err(|_| RuntimeStorageError)?;
        future::pending().await
    }
}

#[async_trait]
impl RuntimeStorage for HungSaveStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        self.save_started
            .send(())
            .map_err(|_| RuntimeStorageError)?;
        future::pending().await
    }
}

#[async_trait]
impl RuntimeStorage for CleanupStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(None)
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        self.calls
            .lock()
            .map_err(|_| RuntimeStorageError)?
            .push("checkpoint");
        Ok(())
    }
}

struct CleanupPlayer {
    calls: Arc<Mutex<Vec<&'static str>>>,
}

struct NotifyingCleanupPlayer {
    calls: Arc<Mutex<Vec<&'static str>>>,
    shutdown_started: mpsc::UnboundedSender<()>,
}

struct HungShutdownPlayer {
    calls: Arc<Mutex<Vec<&'static str>>>,
    shutdown_started: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl RuntimePlayer for HungShutdownPlayer {
    async fn play(
        &self,
        _generation: ytermusic::app::Generation,
        _item: MediaItem,
        _start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn set_volume(&self, _volume: u8) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), RuntimePlayerError> {
        self.calls
            .lock()
            .map_err(|_| RuntimePlayerError)?
            .push("player_shutdown");
        self.shutdown_started
            .send(())
            .map_err(|_| RuntimePlayerError)?;
        future::pending::<Result<(), RuntimePlayerError>>().await
    }

    fn abort(&self) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push("player_abort");
        }
    }
}

#[async_trait]
impl RuntimePlayer for CleanupPlayer {
    async fn play(
        &self,
        _generation: ytermusic::app::Generation,
        _item: MediaItem,
        _start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn set_volume(&self, _volume: u8) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), RuntimePlayerError> {
        self.calls
            .lock()
            .map_err(|_| RuntimePlayerError)?
            .push("player_shutdown");
        Ok(())
    }

    fn abort(&self) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push("player_abort");
        }
    }
}

#[async_trait]
impl RuntimePlayer for NotifyingCleanupPlayer {
    async fn play(
        &self,
        _generation: ytermusic::app::Generation,
        _item: MediaItem,
        _start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn set_volume(&self, _volume: u8) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), RuntimePlayerError> {
        self.calls
            .lock()
            .map_err(|_| RuntimePlayerError)?
            .push("player_shutdown");
        self.shutdown_started
            .send(())
            .map_err(|_| RuntimePlayerError)?;
        Ok(())
    }

    fn abort(&self) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push("player_abort");
        }
    }
}

impl RecordingTerminal {
    fn record(&self, call: &'static str) -> io::Result<()> {
        self.calls
            .lock()
            .map_err(|_| io::Error::other("terminal calls poisoned"))?
            .push(call);
        Ok(())
    }
}

impl TerminalControl for RecordingTerminal {
    fn enter_alternate_screen(&self) -> io::Result<()> {
        self.record("enter_alt")
    }

    fn hide_cursor(&self) -> io::Result<()> {
        self.record("hide_cursor")
    }

    fn enable_mouse_capture(&self) -> io::Result<()> {
        self.record("enable_mouse")
    }

    fn enable_raw_mode(&self) -> io::Result<()> {
        self.record("enable_raw")
    }

    fn disable_raw_mode(&self) -> io::Result<()> {
        self.record("disable_raw")
    }

    fn disable_mouse_capture(&self) -> io::Result<()> {
        self.record("disable_mouse")
    }

    fn show_cursor(&self) -> io::Result<()> {
        self.record("show_cursor")
    }

    fn leave_alternate_screen(&self) -> io::Result<()> {
        self.record("leave_alt")
    }
}

impl TerminalControl for RestorationNotifyingTerminal {
    fn enter_alternate_screen(&self) -> io::Result<()> {
        self.terminal.enter_alternate_screen()
    }

    fn hide_cursor(&self) -> io::Result<()> {
        self.terminal.hide_cursor()
    }

    fn enable_mouse_capture(&self) -> io::Result<()> {
        self.terminal.enable_mouse_capture()
    }

    fn enable_raw_mode(&self) -> io::Result<()> {
        self.terminal.enable_raw_mode()
    }

    fn disable_raw_mode(&self) -> io::Result<()> {
        self.terminal.disable_raw_mode()?;
        self.restored
            .send(())
            .map_err(|_| io::Error::other("terminal restoration observer closed"))
    }

    fn disable_mouse_capture(&self) -> io::Result<()> {
        self.terminal.disable_mouse_capture()
    }

    fn show_cursor(&self) -> io::Result<()> {
        self.terminal.show_cursor()
    }

    fn leave_alternate_screen(&self) -> io::Result<()> {
        self.terminal.leave_alternate_screen()
    }
}

#[async_trait]
impl RuntimePlayer for BlockingVolumePlayer {
    async fn play(
        &self,
        _generation: Generation,
        _item: MediaItem,
        _start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn set_volume(&self, _volume: u8) -> Result<(), RuntimePlayerError> {
        self.started.send(()).map_err(|_| RuntimePlayerError)?;
        self.release
            .acquire()
            .await
            .map_err(|_| RuntimePlayerError)?
            .forget();
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }
}

#[async_trait]
impl RuntimePlayer for SaturatedCleanupPlayer {
    async fn play(
        &self,
        _generation: Generation,
        _item: MediaItem,
        _start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn set_volume(&self, _volume: u8) -> Result<(), RuntimePlayerError> {
        self.started.send(()).map_err(|_| RuntimePlayerError)?;
        self.release
            .acquire()
            .await
            .map_err(|_| RuntimePlayerError)?
            .forget();
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), RuntimePlayerError> {
        self.calls
            .lock()
            .map_err(|_| RuntimePlayerError)?
            .push("player_shutdown");
        future::pending().await
    }

    fn abort(&self) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push("player_abort");
        }
    }
}

#[async_trait]
impl RuntimePlayer for SaturatingFailPlayer {
    async fn play(
        &self,
        _generation: Generation,
        _item: MediaItem,
        _start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn set_volume(&self, _volume: u8) -> Result<(), RuntimePlayerError> {
        self.started.send(()).map_err(|_| RuntimePlayerError)?;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.release_failure
                .acquire()
                .await
                .map_err(|_| RuntimePlayerError)?
                .forget();
            return Err(RuntimePlayerError);
        }
        Ok(())
    }

    async fn shutdown(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }
}

#[async_trait]
impl RuntimePlayer for AcceptingPlayer {
    async fn play(
        &self,
        _generation: ytermusic::app::Generation,
        _item: MediaItem,
        _start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    async fn set_volume(&self, _volume: u8) -> Result<(), RuntimePlayerError> {
        Ok(())
    }
}

#[async_trait]
impl RuntimePlayer for FailingPlayer {
    async fn play(
        &self,
        _generation: ytermusic::app::Generation,
        _item: MediaItem,
        _start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        Err(RuntimePlayerError)
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        Err(RuntimePlayerError)
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        Err(RuntimePlayerError)
    }

    async fn set_volume(&self, _volume: u8) -> Result<(), RuntimePlayerError> {
        Err(RuntimePlayerError)
    }
}

#[async_trait]
impl RuntimePlayer for RecordingPlayer {
    async fn play(
        &self,
        generation: ytermusic::app::Generation,
        item: MediaItem,
        start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        self.calls
            .send(PlayerCall::Play {
                generation,
                item: Box::new(item),
                start_ms,
            })
            .map_err(|_| RuntimePlayerError)
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        self.calls
            .send(PlayerCall::Pause)
            .map_err(|_| RuntimePlayerError)
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        self.calls
            .send(PlayerCall::Resume)
            .map_err(|_| RuntimePlayerError)
    }

    async fn set_volume(&self, volume: u8) -> Result<(), RuntimePlayerError> {
        self.calls
            .send(PlayerCall::Volume(volume))
            .map_err(|_| RuntimePlayerError)
    }

    async fn seek_relative(&self, seconds: i64) -> Result<(), RuntimePlayerError> {
        self.calls
            .send(PlayerCall::SeekRelative(seconds))
            .map_err(|_| RuntimePlayerError)
    }
}

#[async_trait]
impl RuntimeStorage for OrderedPodcastStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        Ok(Some(self.checkpoint.clone()))
    }

    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Ok(())
    }

    async fn save_podcast_progress(
        &self,
        progress: PodcastProgress,
    ) -> Result<(), RuntimeStorageError> {
        *self.progress.lock().map_err(|_| RuntimeStorageError)? = Some(progress.clone());
        self.events
            .send(PodcastStorageEvent::Save(progress))
            .map_err(|_| RuntimeStorageError)
    }

    async fn load_podcast_progress(
        &self,
        video_id: String,
    ) -> Result<Option<PodcastProgress>, RuntimeStorageError> {
        let progress = self
            .progress
            .lock()
            .map_err(|_| RuntimeStorageError)?
            .clone()
            .filter(|progress| progress.video_id == video_id);
        if let Some(progress) = &progress {
            self.events
                .send(PodcastStorageEvent::Load(progress.clone()))
                .map_err(|_| RuntimeStorageError)?;
        }
        Ok(progress)
    }
}

impl Storage for ThreadRecordingStorage {
    fn save_session(
        &mut self,
        _checkpoint: &SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    fn load_session(&self) -> Result<Option<SessionCheckpoint>, StorageError> {
        let _ = self.called.send(thread::current().id());
        Ok(None)
    }

    fn save_podcast_progress(&mut self, _progress: &PodcastProgress) -> Result<(), StorageError> {
        Ok(())
    }

    fn load_podcast_progress(
        &self,
        _video_id: &str,
    ) -> Result<Option<PodcastProgress>, StorageError> {
        Ok(None)
    }

    fn record_history(&mut self, _item: &MediaItem, _played_at: i64) -> Result<(), StorageError> {
        Ok(())
    }

    fn recent_history(&self, _limit: usize) -> Result<Vec<HistoryEntry>, StorageError> {
        Ok(Vec::new())
    }

    fn load_favorites(&self) -> Result<Vec<FavoriteEntry>, StorageError> {
        if let Some(events) = &self.favorite_events {
            let _ = events.send("load");
        }
        Ok(Vec::new())
    }

    fn add_favorite(
        &mut self,
        _item: &MediaItem,
        _favorited_at: i64,
    ) -> Result<FavoriteInsertOutcome, StorageError> {
        if let Some(events) = &self.favorite_events {
            let _ = events.send("add");
        }
        Ok(FavoriteInsertOutcome::Added)
    }

    fn remove_favorite(&mut self, _id: &MediaId) -> Result<bool, StorageError> {
        if let Some(events) = &self.favorite_events {
            let _ = events.send("remove");
        }
        Ok(false)
    }

    fn put_metadata(
        &mut self,
        _cache_key: &str,
        _payload: &str,
        _expires_at: i64,
        _stored_at: i64,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    fn get_metadata_entry(
        &self,
        _cache_key: &str,
    ) -> Result<Option<MetadataCacheEntry>, StorageError> {
        Ok(None)
    }

    fn get_metadata(&self, _cache_key: &str, _now: i64) -> Result<Option<String>, StorageError> {
        Ok(None)
    }
}

impl Storage for BlockingSyncChartStorage {
    fn save_session(
        &mut self,
        _checkpoint: &SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    fn load_session(&self) -> Result<Option<SessionCheckpoint>, StorageError> {
        Ok(None)
    }

    fn save_podcast_progress(&mut self, _progress: &PodcastProgress) -> Result<(), StorageError> {
        Ok(())
    }

    fn load_podcast_progress(
        &self,
        _video_id: &str,
    ) -> Result<Option<PodcastProgress>, StorageError> {
        Ok(None)
    }

    fn record_history(&mut self, _item: &MediaItem, _played_at: i64) -> Result<(), StorageError> {
        Ok(())
    }

    fn recent_history(&self, _limit: usize) -> Result<Vec<HistoryEntry>, StorageError> {
        Ok(Vec::new())
    }

    fn load_favorites(&self) -> Result<Vec<FavoriteEntry>, StorageError> {
        Ok(Vec::new())
    }

    fn add_favorite(
        &mut self,
        _item: &MediaItem,
        _favorited_at: i64,
    ) -> Result<FavoriteInsertOutcome, StorageError> {
        Ok(FavoriteInsertOutcome::Added)
    }

    fn remove_favorite(&mut self, _id: &MediaId) -> Result<bool, StorageError> {
        Ok(false)
    }

    fn put_metadata(
        &mut self,
        _cache_key: &str,
        _payload: &str,
        _expires_at: i64,
        _stored_at: i64,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    fn get_metadata_entry(
        &self,
        _cache_key: &str,
    ) -> Result<Option<MetadataCacheEntry>, StorageError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            let (released, wake) = &*self.gate;
            let mut released = released.lock().map_err(|_| StorageError::CorruptData {
                entity: "test gate",
                reason: "sync storage gate poisoned".to_owned(),
            })?;
            while !*released {
                released = wake.wait(released).map_err(|_| StorageError::CorruptData {
                    entity: "test gate",
                    reason: "sync storage gate poisoned".to_owned(),
                })?;
            }
        }
        Ok(None)
    }

    fn get_metadata(&self, _cache_key: &str, _now: i64) -> Result<Option<String>, StorageError> {
        Ok(None)
    }
}

impl Renderer for RecordingRenderer {
    fn render(&mut self, state: &AppState) -> io::Result<()> {
        self.states
            .lock()
            .map_err(|_| io::Error::other("renderer state poisoned"))?
            .push(state.clone());
        Ok(())
    }
}

fn checkpoint_for(item: MediaItem, status: PlaybackStatus) -> SessionCheckpoint {
    let queue_id = stable_queue_item_id(&item.id);
    SessionCheckpoint {
        queue: QueueSnapshot {
            logical: vec![QueueItem::new(queue_id.clone(), item.clone())],
            active: vec![queue_id.clone()],
            current: Some(queue_id),
            repeat: RepeatMode::Off,
            shuffle_seed: None,
            radio: false,
        },
        playback: PlaybackSnapshot {
            current: Some(item.id),
            status,
            position_ms: 24_000,
            duration_ms: item.duration_ms,
            target_volume: 72,
            playback_speed: 1.0,
        },
    }
}

fn podcast_episode(video_id: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind: MediaKind::PodcastEpisode,
        title: "Restored episode".to_owned(),
        creators: vec!["Host".to_owned()],
        collection: Some("Show".to_owned()),
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}

fn song(video_id: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind: MediaKind::Song,
        title: "Runtime song".to_owned(),
        creators: vec!["Artist".to_owned()],
        collection: None,
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}
