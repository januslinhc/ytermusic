use std::{
    ffi::OsString,
    fmt, io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::Command,
    sync::watch,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    app::Generation,
    domain::MediaId,
    resolver::PreviewStreamUrl,
    ui::artwork::{ArtworkGrid, CellSize, MAX_CELL_HEIGHT, MAX_CELL_WIDTH, MAX_OUTPUT_CELLS},
};

const MAX_ANIMATION_FPS: u8 = 15;
const MAX_ANIMATION_SECONDS: u64 = 4 * 60 * 60;
const CHILD_REAP_GRACE: Duration = Duration::from_millis(50);
const DECODER_REAP_GRACE: Duration = Duration::from_millis(200);
const MAX_FFMPEG_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FFMPEG_PROBE_BYTES: u64 = 1024 * 1024;
const MAX_FFMPEG_ANALYZE_MICROS: u64 = 5_000_000;
const MAX_FFMPEG_SOURCE_PIXELS: u64 = 16 * 1024 * 1024;
const FFMPEG_IO_TIMEOUT_MICROS: u64 = 10_000_000;
const MAX_FFMPEG_PROCESS_RUNTIME: Duration = Duration::from_secs(MAX_ANIMATION_SECONDS + 300);
const MIN_FFMPEG_PROCESS_RUNTIME: Duration = Duration::from_millis(1);

#[derive(Clone, Eq, PartialEq)]
pub struct AnimationKey {
    generation: Generation,
    media_id: MediaId,
    size: CellSize,
}

impl AnimationKey {
    #[must_use]
    pub const fn new(generation: Generation, media_id: MediaId, size: CellSize) -> Self {
        Self {
            generation,
            media_id,
            size,
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn media_id(&self) -> &MediaId {
        &self.media_id
    }

    #[must_use]
    pub const fn size(&self) -> CellSize {
        self.size
    }

    fn valid(&self) -> bool {
        self.size.width > 0
            && self.size.height > 0
            && self.size.width <= MAX_CELL_WIDTH
            && self.size.height <= MAX_CELL_HEIGHT
            && usize::from(self.size.width) * usize::from(self.size.height) <= MAX_OUTPUT_CELLS
    }
}

impl fmt::Debug for AnimationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnimationKey")
            .field("generation", &self.generation)
            .field("media_id", &"[REDACTED]")
            .field("size", &self.size)
            .finish()
    }
}

#[derive(Clone)]
struct AnimationSlot {
    key: AnimationKey,
    lease: AnimationLease,
    frame: Option<Arc<ArtworkGrid>>,
    paused: bool,
    failed: bool,
}

pub struct AnimationFrameStore {
    current: RwLock<Option<AnimationSlot>>,
    redraw: watch::Sender<u64>,
    next_lease: AtomicU64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct AnimationLease(u64);

impl Default for AnimationFrameStore {
    fn default() -> Self {
        Self::new()
    }
}

impl AnimationFrameStore {
    #[must_use]
    pub fn new() -> Self {
        let (redraw, _) = watch::channel(0);
        Self {
            current: RwLock::new(None),
            redraw,
            next_lease: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn subscribe_redraw(&self) -> watch::Receiver<u64> {
        self.redraw.subscribe()
    }

    pub fn request(&self, key: AnimationKey) -> bool {
        self.request_with_lease(key).is_some()
    }

    fn request_with_lease(&self, key: AnimationKey) -> Option<AnimationLease> {
        if !key.valid() {
            self.clear();
            return None;
        }
        let lease = self.allocate_lease()?;
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(AnimationSlot {
            key,
            lease,
            frame: None,
            paused: false,
            failed: false,
        });
        Some(lease)
    }

    pub fn publish(&self, key: &AnimationKey, frame: Arc<ArtworkGrid>) -> bool {
        let lease = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|slot| slot.key == *key)
            .map(|slot| slot.lease);
        lease.is_some_and(|lease| self.publish_with_lease(key, lease, frame))
    }

    fn publish_with_lease(
        &self,
        key: &AnimationKey,
        lease: AnimationLease,
        frame: Arc<ArtworkGrid>,
    ) -> bool {
        if frame.width() != key.size.width || frame.height() != key.size.height {
            return false;
        }
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = current
            .as_mut()
            .filter(|slot| slot.key == *key && slot.lease == lease && !slot.paused && !slot.failed)
        else {
            return false;
        };
        slot.frame = Some(frame);
        drop(current);
        self.redraw.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
        true
    }

    #[must_use]
    pub fn presentation(&self, key: &AnimationKey) -> Option<Arc<ArtworkGrid>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|slot| slot.key == *key && !slot.failed)
            .and_then(|slot| slot.frame.clone())
    }

    pub fn pause(&self, key: &AnimationKey) -> bool {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = current
            .as_mut()
            .filter(|slot| slot.key == *key && !slot.failed)
        else {
            return false;
        };
        slot.paused = true;
        true
    }

    pub fn resume(&self, key: &AnimationKey) -> bool {
        self.resume_with_new_lease(key).is_some()
    }

    fn resume_with_new_lease(&self, key: &AnimationKey) -> Option<AnimationLease> {
        let lease = self.allocate_lease()?;
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = current
            .as_mut()
            .filter(|slot| slot.key == *key && !slot.failed)?;
        slot.lease = lease;
        slot.paused = false;
        Some(lease)
    }

    pub fn fail(&self, key: &AnimationKey) -> bool {
        let lease = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|slot| slot.key == *key)
            .map(|slot| slot.lease);
        lease.is_some_and(|lease| self.fail_with_lease(key, lease))
    }

    fn fail_with_lease(&self, key: &AnimationKey, lease: AnimationLease) -> bool {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = current
            .as_mut()
            .filter(|slot| slot.key == *key && slot.lease == lease)
        else {
            return false;
        };
        slot.frame = None;
        slot.failed = true;
        true
    }

    pub fn clear(&self) {
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn allocate_lease(&self) -> Option<AnimationLease> {
        self.next_lease
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |lease| {
                lease.checked_add(1)
            })
            .ok()
            .and_then(|lease| lease.checked_add(1))
            .map(AnimationLease)
    }
}

impl fmt::Debug for AnimationFrameStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let current = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("AnimationFrameStore")
            .field(
                "generation",
                &current.as_ref().map(|slot| slot.key.generation),
            )
            .field(
                "has_frame",
                &current.as_ref().is_some_and(|slot| slot.frame.is_some()),
            )
            .field("paused", &current.as_ref().is_some_and(|slot| slot.paused))
            .field("failed", &current.as_ref().is_some_and(|slot| slot.failed))
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct AnimationRequest {
    key: AnimationKey,
    preview_url: PreviewStreamUrl,
    max_fps: u8,
    start_ms: u64,
}

impl AnimationRequest {
    #[must_use]
    pub const fn new(key: AnimationKey, preview_url: PreviewStreamUrl) -> Self {
        Self {
            key,
            preview_url,
            max_fps: 8,
            start_ms: 0,
        }
    }

    #[must_use]
    pub const fn key(&self) -> &AnimationKey {
        &self.key
    }

    fn with_max_fps(mut self, max_fps: u8) -> Self {
        self.max_fps = max_fps;
        self
    }

    pub(crate) fn with_start_ms(mut self, start_ms: u64) -> Self {
        self.start_ms = start_ms;
        self
    }
}

impl fmt::Debug for AnimationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnimationRequest")
            .field("key", &self.key)
            .field("preview_url", &"[REDACTED]")
            .field("max_fps", &self.max_fps)
            .field("start_ms", &self.start_ms)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AnimationError {
    #[error("animation decoder is unavailable")]
    Unavailable,
    #[error("animation frame violated a resource limit")]
    ResourceLimit,
    #[error("animation decoding failed")]
    DecodeFailed,
}

pub type AnimationFrameOutput = Result<Arc<ArtworkGrid>, AnimationError>;

#[async_trait]
pub trait AnimationDecoder: Send + Sync {
    /// Decodes a bounded preview and replaces `output` with the newest frame.
    /// Implementations must stop and reap owned resources when `cancel` fires.
    async fn decode(
        &self,
        request: AnimationRequest,
        output: watch::Sender<Option<AnimationFrameOutput>>,
        cancel: CancellationToken,
    ) -> Result<(), AnimationError>;
}

#[async_trait]
pub trait AnimationPacer: Send + Sync {
    async fn wait(&self, duration: Duration);
}

#[derive(Debug, Default)]
pub struct TokioAnimationPacer;

#[async_trait]
impl AnimationPacer for TokioAnimationPacer {
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

struct ActiveAnimation {
    key: AnimationKey,
    request: AnimationRequest,
    cancel: CancellationToken,
    decoder: JoinHandle<()>,
    publisher: JoinHandle<()>,
}

impl ActiveAnimation {
    fn retire(self) -> JoinHandle<()> {
        self.cancel.cancel();
        tokio::spawn(async move {
            let mut decoder = self.decoder;
            let mut publisher = self.publisher;
            let graceful = tokio::time::timeout(DECODER_REAP_GRACE, async {
                let _ = tokio::join!(&mut decoder, &mut publisher);
            })
            .await
            .is_ok();
            if !graceful {
                decoder.abort();
                publisher.abort();
                let _ = decoder.await;
                let _ = publisher.await;
            }
        })
    }
}

pub struct AnimationWorker {
    decoder: Arc<dyn AnimationDecoder>,
    pacer: Arc<dyn AnimationPacer>,
    store: Arc<AnimationFrameStore>,
    max_fps: u8,
    active: Option<ActiveAnimation>,
    paused: Option<AnimationRequest>,
    retiring: Vec<JoinHandle<()>>,
}

impl AnimationWorker {
    #[must_use]
    pub fn spawn(
        decoder: Arc<dyn AnimationDecoder>,
        pacer: Arc<dyn AnimationPacer>,
        store: Arc<AnimationFrameStore>,
        max_fps: u8,
    ) -> Self {
        Self {
            decoder,
            pacer,
            store,
            max_fps: max_fps.clamp(1, MAX_ANIMATION_FPS),
            active: None,
            paused: None,
            retiring: Vec::new(),
        }
    }

    pub fn replace(&mut self, request: AnimationRequest) {
        self.retire_active();
        self.paused = None;
        self.prune_retired();
        let request = request.with_max_fps(self.max_fps);
        let key = request.key.clone();
        let Some(lease) = self.store.request_with_lease(key) else {
            return;
        };
        self.start(request, lease);
    }

    fn start(&mut self, request: AnimationRequest, lease: AnimationLease) {
        let key = request.key.clone();
        let cancel = CancellationToken::new();
        let (output, output_rx) = watch::channel(None);
        let decoder = Arc::clone(&self.decoder);
        let decoder_cancel = cancel.clone();
        let error_output = output.clone();
        let decoder_request = request.clone();
        let decoder_task = tokio::spawn(async move {
            if let Err(error) = decoder
                .decode(decoder_request, output, decoder_cancel)
                .await
            {
                error_output.send_replace(Some(Err(error)));
            }
        });
        let publisher = tokio::spawn(publish_latest_frames(
            key.clone(),
            Arc::clone(&self.store),
            Arc::clone(&self.pacer),
            output_rx,
            cancel.clone(),
            self.max_fps,
            lease,
        ));
        self.active = Some(ActiveAnimation {
            key,
            request,
            cancel,
            decoder: decoder_task,
            publisher,
        });
    }

    pub fn pause(&mut self) {
        self.prune_retired();
        if let Some(active) = self.active.take() {
            let _ = self.store.pause(&active.key);
            self.paused = Some(active.request.clone());
            self.retiring.push(active.retire());
        }
    }

    #[must_use]
    pub fn active_key(&self) -> Option<&AnimationKey> {
        self.active
            .as_ref()
            .map(|active| &active.key)
            .or_else(|| self.paused.as_ref().map(|request| &request.key))
    }

    pub fn resume(&mut self, position_ms: u64) {
        self.prune_retired();
        if self.active.is_some() {
            return;
        }
        if let Some(request) = self.paused.take() {
            let request = request.with_start_ms(position_ms);
            if let Some(lease) = self.store.resume_with_new_lease(&request.key) {
                self.start(request, lease);
            }
        }
    }

    #[must_use]
    pub fn redraw_receiver(&self) -> watch::Receiver<u64> {
        self.store.subscribe_redraw()
    }

    pub fn clear(&mut self) {
        self.retire_active();
        self.paused = None;
        self.store.clear();
    }

    pub async fn shutdown(&mut self) {
        self.clear();
        for task in self.retiring.drain(..) {
            let _ = task.await;
        }
    }

    fn retire_active(&mut self) {
        if let Some(active) = self.active.take() {
            self.retiring.push(active.retire());
        }
    }

    fn prune_retired(&mut self) {
        self.retiring.retain(|task| !task.is_finished());
    }
}

impl Drop for AnimationWorker {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancel.cancel();
            // Dropping join handles detaches the tasks so the decoder retains
            // ownership until its bounded process cleanup completes.
            drop(active);
        }
    }
}

async fn publish_latest_frames(
    key: AnimationKey,
    store: Arc<AnimationFrameStore>,
    pacer: Arc<dyn AnimationPacer>,
    mut output: watch::Receiver<Option<AnimationFrameOutput>>,
    cancel: CancellationToken,
    max_fps: u8,
    lease: AnimationLease,
) {
    let interval = Duration::from_secs_f64(1.0 / f64::from(max_fps));
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            changed = output.changed() => {
                if changed.is_err() { return; }
                let latest = output.borrow_and_update().clone();
                match latest {
                    Some(Ok(frame)) => {
                        if store.publish_with_lease(&key, lease, frame) { pacer.wait(interval).await; }
                    }
                    Some(Err(_)) => { let _ = store.fail_with_lease(&key, lease); return; }
                    None => {}
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct FfmpegAnimationDecoder {
    executable: PathBuf,
    process_timeout: Duration,
    launcher: Arc<dyn AnimationProcessLauncher>,
}

impl FfmpegAnimationDecoder {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            process_timeout: MAX_FFMPEG_PROCESS_RUNTIME,
            launcher: Arc::new(TokioAnimationProcessLauncher),
        }
    }

    #[must_use]
    pub fn with_process_timeout(mut self, timeout: Duration) -> Self {
        self.process_timeout =
            timeout.clamp(MIN_FFMPEG_PROCESS_RUNTIME, MAX_FFMPEG_PROCESS_RUNTIME);
        self
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[cfg(test)]
    fn with_launcher(mut self, launcher: Arc<dyn AnimationProcessLauncher>) -> Self {
        self.launcher = launcher;
        self
    }
}

impl fmt::Debug for FfmpegAnimationDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FfmpegAnimationDecoder { executable: [REDACTED] }")
    }
}

#[async_trait]
impl AnimationDecoder for FfmpegAnimationDecoder {
    async fn decode(
        &self,
        request: AnimationRequest,
        output: watch::Sender<Option<AnimationFrameOutput>>,
        cancel: CancellationToken,
    ) -> Result<(), AnimationError> {
        if !request.key.valid() {
            return Err(AnimationError::ResourceLimit);
        }
        let size = request.key.size;
        let mut child = self
            .launcher
            .spawn(&self.executable, &ffmpeg_arguments(&request))
            .map_err(|_| AnimationError::Unavailable)?;
        let mut stdout = child.take_stdout().ok_or(AnimationError::Unavailable)?;
        let deadline = tokio::time::Instant::now() + self.process_timeout;
        let frame_bytes = usize::from(size.width)
            .checked_mul(usize::from(size.height))
            .and_then(|cells| cells.checked_mul(6))
            .ok_or(AnimationError::ResourceLimit)?;
        let frame_limit = u64::from(request.max_fps) * MAX_ANIMATION_SECONDS;
        let mut frame = vec![0_u8; frame_bytes];
        for _ in 0..frame_limit {
            let read = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    kill_and_wait(child).await?;
                    return Ok(());
                }
                () = tokio::time::sleep_until(deadline) => {
                    kill_and_wait(child).await?;
                    return Err(AnimationError::ResourceLimit);
                }
                result = stdout.read_exact(&mut frame) => result,
            };
            match read {
                Ok(_) => {
                    let Ok(grid) = crate::ui::artwork::decode_rgb_frame(&frame, size) else {
                        kill_and_wait(child).await?;
                        return Err(AnimationError::DecodeFailed);
                    };
                    output.send_replace(Some(Ok(Arc::new(grid))));
                }
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(_) => {
                    kill_and_wait(child).await?;
                    return Err(AnimationError::DecodeFailed);
                }
            }
        }
        drop(stdout);
        let status = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                kill_and_wait(child).await?;
                return Ok(());
            }
            () = tokio::time::sleep_until(deadline) => {
                kill_and_wait(child).await?;
                return Err(AnimationError::ResourceLimit);
            }
            result = child.wait() => result.map_err(|_| AnimationError::DecodeFailed)?,
        };
        if status {
            Ok(())
        } else {
            Err(AnimationError::DecodeFailed)
        }
    }
}

async fn kill_and_wait(mut child: Box<dyn AnimationProcess>) -> Result<(), AnimationError> {
    let _ = child.start_kill();
    match tokio::time::timeout(CHILD_REAP_GRACE, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err(AnimationError::ResourceLimit),
        Err(_) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            Err(AnimationError::ResourceLimit)
        }
    }
}

type AnimationProcessStdout = Pin<Box<dyn AsyncRead + Send + 'static>>;

#[async_trait]
trait AnimationProcess: Send {
    fn take_stdout(&mut self) -> Option<AnimationProcessStdout>;
    fn start_kill(&mut self) -> io::Result<()>;
    async fn wait(&mut self) -> io::Result<bool>;
}

trait AnimationProcessLauncher: Send + Sync {
    fn spawn(&self, executable: &Path, args: &[OsString]) -> io::Result<Box<dyn AnimationProcess>>;
}

struct TokioAnimationProcessLauncher;

impl AnimationProcessLauncher for TokioAnimationProcessLauncher {
    fn spawn(&self, executable: &Path, args: &[OsString]) -> io::Result<Box<dyn AnimationProcess>> {
        let child = Command::new(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Box::new(TokioAnimationProcess { child }))
    }
}

struct TokioAnimationProcess {
    child: tokio::process::Child,
}

#[async_trait]
impl AnimationProcess for TokioAnimationProcess {
    fn take_stdout(&mut self) -> Option<AnimationProcessStdout> {
        self.child
            .stdout
            .take()
            .map(|stdout| Box::pin(stdout) as AnimationProcessStdout)
    }

    fn start_kill(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }

    async fn wait(&mut self) -> io::Result<bool> {
        self.child.wait().await.map(|status| status.success())
    }
}

fn ffmpeg_arguments(request: &AnimationRequest) -> Vec<OsString> {
    let size = request.key.size;
    let pixel_height = u32::from(size.height) * 2;
    let filter = format!(
        "fps={},scale={}:{}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2",
        request.max_fps, size.width, pixel_height, size.width, pixel_height
    );
    [
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-max_alloc".into(),
        MAX_FFMPEG_ALLOC_BYTES.to_string().into(),
        "-probesize".into(),
        MAX_FFMPEG_PROBE_BYTES.to_string().into(),
        "-analyzeduration".into(),
        MAX_FFMPEG_ANALYZE_MICROS.to_string().into(),
        "-rw_timeout".into(),
        FFMPEG_IO_TIMEOUT_MICROS.to_string().into(),
        "-max_pixels".into(),
        MAX_FFMPEG_SOURCE_PIXELS.to_string().into(),
        "-readrate".into(),
        "1".into(),
        "-ss".into(),
        format!(
            "{}.{:03}",
            request.start_ms / 1_000,
            request.start_ms % 1_000
        )
        .into(),
        "-i".into(),
        request.preview_url.as_url().as_str().into(),
        "-an".into(),
        "-t".into(),
        MAX_ANIMATION_SECONDS.to_string().into(),
        "-vf".into(),
        filter.into(),
        "-pix_fmt".into(),
        "rgb24".into(),
        "-f".into(),
        "rawvideo".into(),
        "pipe:1".into(),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use async_trait::async_trait;
    use tokio::io::{AsyncRead, ReadBuf};
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use crate::{
        app::Generation,
        domain::MediaId,
        ui::artwork::{CellSize, decode_rgb_frame},
    };

    use super::{
        AnimationDecoder, AnimationError, AnimationFrameOutput, AnimationFrameStore, AnimationKey,
        AnimationPacer, AnimationRequest, AnimationWorker, MAX_ANIMATION_SECONDS,
        MAX_FFMPEG_ALLOC_BYTES, MAX_FFMPEG_PROCESS_RUNTIME, MAX_FFMPEG_SOURCE_PIXELS,
        MIN_FFMPEG_PROCESS_RUNTIME, ffmpeg_arguments,
    };

    fn key(generation: u64, media: &str, size: CellSize) -> AnimationKey {
        AnimationKey::new(
            Generation::new(generation),
            MediaId {
                provider: "youtube".to_owned(),
                video_id: media.to_owned(),
            },
            size,
        )
    }

    fn grid(size: CellSize, red: u8) -> Arc<crate::ui::artwork::ArtworkGrid> {
        let pixels = vec![red; usize::from(size.width) * usize::from(size.height) * 2 * 3];
        Arc::new(
            decode_rgb_frame(&pixels, size)
                .unwrap_or_else(|error| panic!("bounded test frame must decode: {error}")),
        )
    }

    #[test]
    fn newest_frame_replaces_the_only_presentation_slot() {
        let store = AnimationFrameStore::new();
        let key = key(1, "video-a", CellSize::new(2, 1));
        assert!(store.request(key.clone()));
        let first = grid(key.size(), 10);
        let newest = grid(key.size(), 20);

        assert!(store.publish(&key, Arc::clone(&first)));
        assert!(store.publish(&key, Arc::clone(&newest)));

        let shown = store
            .presentation(&key)
            .unwrap_or_else(|| panic!("frame missing"));
        assert!(Arc::ptr_eq(&shown, &newest));
    }

    #[test]
    fn generation_media_and_cell_size_must_all_match() {
        let store = AnimationFrameStore::new();
        let current = key(7, "video-a", CellSize::new(2, 1));
        assert!(store.request(current.clone()));

        for stale in [
            key(6, "video-a", CellSize::new(2, 1)),
            key(7, "video-b", CellSize::new(2, 1)),
            key(7, "video-a", CellSize::new(1, 1)),
        ] {
            assert!(!store.publish(&stale, grid(stale.size(), 1)));
        }
        assert!(store.presentation(&current).is_none());
    }

    #[test]
    fn empty_or_oversized_targets_are_rejected() {
        let store = AnimationFrameStore::new();
        assert!(!store.request(key(1, "a", CellSize::new(0, 1))));
        assert!(!store.request(key(1, "a", CellSize::new(257, 1))));
        assert!(
            store
                .presentation(&key(1, "a", CellSize::new(0, 1)))
                .is_none()
        );
    }

    #[test]
    fn pause_blocks_publication_and_resume_accepts_new_frames() {
        let store = AnimationFrameStore::new();
        let key = key(1, "video-a", CellSize::new(2, 1));
        assert!(store.request(key.clone()));
        assert!(store.publish(&key, grid(key.size(), 10)));
        store.pause(&key);

        assert!(!store.publish(&key, grid(key.size(), 20)));
        assert_eq!(
            store
                .presentation(&key)
                .map(|frame| frame.cells()[0].foreground().red()),
            Some(10)
        );

        assert!(store.resume(&key));
        assert!(store.publish(&key, grid(key.size(), 30)));
        assert_eq!(
            store
                .presentation(&key)
                .map(|frame| frame.cells()[0].foreground().red()),
            Some(30)
        );
    }

    #[test]
    fn same_key_restart_rejects_retiring_decoder_frames_and_failures() {
        let store = AnimationFrameStore::new();
        let key = key(1, "video-a", CellSize::new(2, 1));
        let old_lease = store
            .request_with_lease(key.clone())
            .unwrap_or_else(|| panic!("valid request must allocate a lease"));
        assert!(store.publish_with_lease(&key, old_lease, grid(key.size(), 10)));
        assert!(store.pause(&key));
        let current_lease = store
            .resume_with_new_lease(&key)
            .unwrap_or_else(|| panic!("valid resume must allocate a lease"));

        assert!(!store.publish_with_lease(&key, old_lease, grid(key.size(), 20)));
        assert!(!store.fail_with_lease(&key, old_lease));
        assert!(store.publish_with_lease(&key, current_lease, grid(key.size(), 30)));
        assert_eq!(
            store
                .presentation(&key)
                .map(|frame| frame.cells()[0].foreground().red()),
            Some(30)
        );
    }

    #[test]
    fn failure_clears_animation_so_static_artwork_can_fall_back() {
        let store = AnimationFrameStore::new();
        let key = key(1, "video-a", CellSize::new(2, 1));
        assert!(store.request(key.clone()));
        assert!(store.publish(&key, grid(key.size(), 10)));

        assert!(store.fail(&key));

        assert!(store.presentation(&key).is_none());
        assert!(!store.publish(&key, grid(key.size(), 20)));
    }

    #[test]
    fn debug_output_redacts_media_identity_and_frame_content() {
        let store = AnimationFrameStore::new();
        let key = key(1, "secret-video-id", CellSize::new(2, 1));
        assert!(store.request(key));
        let debug = format!("{store:?}");
        assert!(!debug.contains("secret-video-id"));
        assert!(!debug.contains("youtube"));
    }

    #[test]
    fn ffmpeg_uses_bounded_direct_arguments_and_one_opaque_input_argument() {
        let secret = "https://video.invalid/preview?token=secret&next=-vf";
        let request = AnimationRequest::new(
            key(1, "video-a", CellSize::new(21, 8)),
            crate::resolver::PreviewStreamUrl::parse(secret)
                .unwrap_or_else(|error| panic!("valid preview: {error}")),
        );
        let args = ffmpeg_arguments(&request);
        let strings = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();

        assert_eq!(
            strings.iter().filter(|arg| arg.as_ref() == secret).count(),
            1
        );
        assert!(
            strings
                .windows(2)
                .any(|pair| pair == ["-max_alloc", &MAX_FFMPEG_ALLOC_BYTES.to_string()])
        );
        assert!(
            strings
                .windows(2)
                .any(|pair| pair == ["-max_pixels", &MAX_FFMPEG_SOURCE_PIXELS.to_string()])
        );
        assert!(
            strings
                .windows(2)
                .any(|pair| pair == ["-rw_timeout", "10000000"])
        );
        assert!(
            strings
                .windows(2)
                .any(|pair| pair == ["-t", &MAX_ANIMATION_SECONDS.to_string()])
        );
        assert!(strings.windows(2).any(|pair| pair == ["-f", "rawvideo"]));
        assert!(strings.windows(2).any(|pair| pair == ["-readrate", "1"]));
        assert!(
            strings
                .iter()
                .all(|arg| arg.as_ref() != "sh" && arg.as_ref() != "-c")
        );
    }

    #[test]
    fn ffmpeg_process_wall_clock_limit_is_injected_and_strictly_bounded() {
        let minimum =
            super::FfmpegAnimationDecoder::new("ffmpeg").with_process_timeout(Duration::ZERO);
        let maximum =
            super::FfmpegAnimationDecoder::new("ffmpeg").with_process_timeout(Duration::MAX);

        assert_eq!(minimum.process_timeout, MIN_FFMPEG_PROCESS_RUNTIME);
        assert_eq!(maximum.process_timeout, MAX_FFMPEG_PROCESS_RUNTIME);
    }

    struct PendingReader;

    impl AsyncRead for PendingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    struct FakeProcessLauncher {
        pending_stdout: bool,
        block_wait_until_killed: bool,
        never_finish_wait: bool,
        wait_gate: Option<Arc<tokio::sync::Notify>>,
        killed: Arc<AtomicUsize>,
        waited: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    impl super::AnimationProcessLauncher for FakeProcessLauncher {
        fn spawn(
            &self,
            _executable: &std::path::Path,
            _args: &[std::ffi::OsString],
        ) -> io::Result<Box<dyn super::AnimationProcess>> {
            let stdout: super::AnimationProcessStdout = if self.pending_stdout {
                Box::pin(PendingReader)
            } else {
                Box::pin(tokio::io::empty())
            };
            Ok(Box::new(FakeProcess {
                stdout: Some(stdout),
                block_wait_until_killed: self.block_wait_until_killed,
                never_finish_wait: self.never_finish_wait,
                wait_gate: self.wait_gate.clone(),
                killed: Arc::clone(&self.killed),
                waited: Arc::clone(&self.waited),
                dropped: Arc::clone(&self.dropped),
            }))
        }
    }

    struct FakeProcess {
        stdout: Option<super::AnimationProcessStdout>,
        block_wait_until_killed: bool,
        never_finish_wait: bool,
        wait_gate: Option<Arc<tokio::sync::Notify>>,
        killed: Arc<AtomicUsize>,
        waited: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for FakeProcess {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl super::AnimationProcess for FakeProcess {
        fn take_stdout(&mut self) -> Option<super::AnimationProcessStdout> {
            self.stdout.take()
        }

        fn start_kill(&mut self) -> io::Result<()> {
            self.killed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn wait(&mut self) -> io::Result<bool> {
            self.waited.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.wait_gate {
                gate.notified().await;
                return Ok(false);
            }
            if self.never_finish_wait
                || (self.block_wait_until_killed && self.killed.load(Ordering::SeqCst) == 0)
            {
                std::future::pending().await
            } else {
                Ok(false)
            }
        }
    }

    fn ffmpeg_test_request() -> AnimationRequest {
        AnimationRequest::new(
            key(1, "video-a", CellSize::new(2, 1)),
            crate::resolver::PreviewStreamUrl::parse("https://video.invalid/preview")
                .unwrap_or_else(|error| panic!("valid preview: {error}")),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn ffmpeg_wall_clock_timeout_kills_and_reaps_stalled_process_without_real_sleep() {
        let killed = Arc::new(AtomicUsize::new(0));
        let waited = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let decoder = Arc::new(
            super::FfmpegAnimationDecoder::new("ffmpeg")
                .with_process_timeout(Duration::from_millis(5))
                .with_launcher(Arc::new(FakeProcessLauncher {
                    pending_stdout: true,
                    block_wait_until_killed: false,
                    never_finish_wait: false,
                    wait_gate: None,
                    killed: Arc::clone(&killed),
                    waited: Arc::clone(&waited),
                    dropped: Arc::clone(&dropped),
                })),
        );
        let (output, _frames) = watch::channel(None);
        let task = tokio::spawn(async move {
            decoder
                .decode(ffmpeg_test_request(), output, CancellationToken::new())
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(5)).await;

        assert_eq!(
            task.await.unwrap_or_else(|error| panic!("join: {error}")),
            Err(AnimationError::ResourceLimit)
        );
        assert_eq!(killed.load(Ordering::SeqCst), 1);
        assert_eq!(waited.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ffmpeg_final_wait_observes_cancellation_then_kills_and_reaps() {
        let killed = Arc::new(AtomicUsize::new(0));
        let waited = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let decoder = Arc::new(super::FfmpegAnimationDecoder::new("ffmpeg").with_launcher(
            Arc::new(FakeProcessLauncher {
                pending_stdout: false,
                block_wait_until_killed: true,
                never_finish_wait: false,
                wait_gate: None,
                killed: Arc::clone(&killed),
                waited: Arc::clone(&waited),
                dropped: Arc::clone(&dropped),
            }),
        ));
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (output, _frames) = watch::channel(None);
        let task = tokio::spawn(async move {
            decoder
                .decode(ffmpeg_test_request(), output, task_cancel)
                .await
        });
        while waited.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();

        assert_eq!(
            task.await.unwrap_or_else(|error| panic!("join: {error}")),
            Ok(())
        );
        assert_eq!(killed.load(Ordering::SeqCst), 1);
        assert_eq!(waited.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ffmpeg_bounds_noncooperative_wait_after_kill() {
        let killed = Arc::new(AtomicUsize::new(0));
        let waited = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let wait_gate = Arc::new(tokio::sync::Notify::new());
        let decoder = super::FfmpegAnimationDecoder::new("ffmpeg").with_launcher(Arc::new(
            FakeProcessLauncher {
                pending_stdout: true,
                block_wait_until_killed: false,
                never_finish_wait: false,
                wait_gate: Some(Arc::clone(&wait_gate)),
                killed: Arc::clone(&killed),
                waited: Arc::clone(&waited),
                dropped: Arc::clone(&dropped),
            },
        ));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (output, _frames) = watch::channel(None);

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            decoder.decode(ffmpeg_test_request(), output, cancel),
        )
        .await;

        assert!(result.is_ok(), "kill-and-reap exceeded its resource bound");
        assert_eq!(result.unwrap_or(Ok(())), Err(AnimationError::ResourceLimit));
        assert_eq!(killed.load(Ordering::SeqCst), 1);
        while waited.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        wait_gate.notify_one();
        while dropped.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[derive(Default)]
    struct RecordingPacer {
        waits: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl AnimationPacer for RecordingPacer {
        async fn wait(&self, duration: Duration) {
            self.waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(duration);
            tokio::task::yield_now().await;
        }
    }

    struct BurstDecoder {
        frames: Vec<Arc<crate::ui::artwork::ArtworkGrid>>,
    }

    #[async_trait]
    impl AnimationDecoder for BurstDecoder {
        async fn decode(
            &self,
            _request: AnimationRequest,
            output: watch::Sender<Option<AnimationFrameOutput>>,
            _cancel: CancellationToken,
        ) -> Result<(), AnimationError> {
            for frame in &self.frames {
                output.send_replace(Some(Ok(Arc::clone(frame))));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn worker_paces_published_frames_at_configured_maximum() {
        let size = CellSize::new(2, 1);
        let store = Arc::new(AnimationFrameStore::new());
        let pacer = Arc::new(RecordingPacer::default());
        let decoder = Arc::new(BurstDecoder {
            frames: vec![grid(size, 10), grid(size, 20)],
        });
        let mut worker = AnimationWorker::spawn(decoder, pacer.clone(), Arc::clone(&store), 8);
        let request = AnimationRequest::new(
            key(1, "video-a", size),
            crate::resolver::PreviewStreamUrl::parse("https://video.invalid/preview")
                .unwrap_or_else(|error| panic!("valid preview: {error}")),
        );

        worker.replace(request);
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        worker.shutdown().await;

        let waits = pacer
            .waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(waits.iter().all(|wait| *wait == Duration::from_millis(125)));
        assert!(!waits.is_empty());
    }

    #[tokio::test]
    async fn burst_decoder_keeps_the_newest_frame_without_backpressure() {
        let size = CellSize::new(2, 1);
        let store = Arc::new(AnimationFrameStore::new());
        let decoder = Arc::new(BurstDecoder {
            frames: vec![grid(size, 10), grid(size, 20), grid(size, 30)],
        });
        let mut worker = AnimationWorker::spawn(
            decoder,
            Arc::new(RecordingPacer::default()),
            Arc::clone(&store),
            8,
        );
        let key = key(1, "video-a", size);
        worker.replace(AnimationRequest::new(
            key.clone(),
            crate::resolver::PreviewStreamUrl::parse("https://video.invalid/preview")
                .unwrap_or_else(|error| panic!("valid preview: {error}")),
        ));

        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            store
                .presentation(&key)
                .map(|frame| frame.cells()[0].foreground().red()),
            Some(30)
        );
        worker.shutdown().await;
    }

    struct ControlledDecoder {
        frames: tokio::sync::Mutex<
            tokio::sync::mpsc::UnboundedReceiver<Arc<crate::ui::artwork::ArtworkGrid>>,
        >,
    }

    #[async_trait]
    impl AnimationDecoder for ControlledDecoder {
        async fn decode(
            &self,
            _request: AnimationRequest,
            output: watch::Sender<Option<AnimationFrameOutput>>,
            cancel: CancellationToken,
        ) -> Result<(), AnimationError> {
            let mut frames = self.frames.lock().await;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ok(()),
                    frame = frames.recv() => match frame {
                        Some(frame) => { output.send_replace(Some(Ok(frame))); }
                        None => return Ok(()),
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn worker_pause_prevents_publication_and_resume_publishes_latest_frame() {
        let size = CellSize::new(2, 1);
        let store = Arc::new(AnimationFrameStore::new());
        let (frames, frame_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut worker = AnimationWorker::spawn(
            Arc::new(ControlledDecoder {
                frames: tokio::sync::Mutex::new(frame_rx),
            }),
            Arc::new(RecordingPacer::default()),
            Arc::clone(&store),
            8,
        );
        let key = key(1, "video-a", size);
        worker.replace(AnimationRequest::new(
            key.clone(),
            crate::resolver::PreviewStreamUrl::parse("https://video.invalid/preview")
                .unwrap_or_else(|error| panic!("valid preview: {error}")),
        ));
        frames
            .send(grid(size, 10))
            .unwrap_or_else(|_| panic!("decoder closed"));
        while store.presentation(&key).is_none() {
            tokio::task::yield_now().await;
        }
        worker.pause();
        frames
            .send(grid(size, 20))
            .unwrap_or_else(|_| panic!("decoder closed"));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            store
                .presentation(&key)
                .map(|frame| frame.cells()[0].foreground().red()),
            Some(10)
        );
        worker.resume(1_000);
        while store
            .presentation(&key)
            .is_some_and(|frame| frame.cells()[0].foreground().red() != 20)
        {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            store
                .presentation(&key)
                .map(|frame| frame.cells()[0].foreground().red()),
            Some(20)
        );
        worker.shutdown().await;
    }

    struct RequestRecordingDecoder {
        requests: tokio::sync::mpsc::UnboundedSender<u64>,
    }

    #[async_trait]
    impl AnimationDecoder for RequestRecordingDecoder {
        async fn decode(
            &self,
            request: AnimationRequest,
            _output: watch::Sender<Option<AnimationFrameOutput>>,
            cancel: CancellationToken,
        ) -> Result<(), AnimationError> {
            let _ = self.requests.send(request.start_ms);
            cancel.cancelled().await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn pause_stops_decode_and_resume_restarts_at_current_media_position() {
        let (requests, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut worker = AnimationWorker::spawn(
            Arc::new(RequestRecordingDecoder { requests }),
            Arc::new(RecordingPacer::default()),
            Arc::new(AnimationFrameStore::new()),
            8,
        );
        worker.replace(ffmpeg_test_request());
        assert_eq!(request_rx.recv().await, Some(0));

        worker.pause();
        worker.resume(42_375);

        assert_eq!(request_rx.recv().await, Some(42_375));
        worker.shutdown().await;
    }

    #[tokio::test]
    async fn repeated_pause_resume_prunes_completed_retirement_bookkeeping() {
        let (requests, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut worker = AnimationWorker::spawn(
            Arc::new(RequestRecordingDecoder { requests }),
            Arc::new(RecordingPacer::default()),
            Arc::new(AnimationFrameStore::new()),
            8,
        );
        worker.replace(ffmpeg_test_request());
        assert_eq!(request_rx.recv().await, Some(0));

        for position_ms in 1..=32 {
            worker.pause();
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            worker.resume(position_ms);
            assert_eq!(request_rx.recv().await, Some(position_ms));
        }

        assert!(
            worker.retiring.len() <= 1,
            "completed pause retirements accumulated: {}",
            worker.retiring.len()
        );
        worker.shutdown().await;
    }

    #[test]
    fn redraw_notifications_are_nonblocking_coalesced_and_ignore_stale_frames() {
        let store = AnimationFrameStore::new();
        let current = key(7, "video-a", CellSize::new(2, 1));
        let stale = key(6, "video-a", CellSize::new(2, 1));
        assert!(store.request(current.clone()));
        let mut redraw = store.subscribe_redraw();

        for red in 0..=u8::MAX {
            assert!(store.publish(&current, grid(current.size(), red)));
        }
        assert!(!store.publish(&stale, grid(stale.size(), 1)));

        assert!(redraw.has_changed().unwrap_or(false));
        assert_eq!(*redraw.borrow_and_update(), 256);
        assert!(!redraw.has_changed().unwrap_or(true));
    }

    struct BlockingDecoder {
        started: Arc<AtomicUsize>,
        reaped: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AnimationDecoder for BlockingDecoder {
        async fn decode(
            &self,
            _request: AnimationRequest,
            _output: watch::Sender<Option<AnimationFrameOutput>>,
            cancel: CancellationToken,
        ) -> Result<(), AnimationError> {
            self.started.fetch_add(1, Ordering::SeqCst);
            cancel.cancelled().await;
            self.reaped.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn replacement_cancels_and_reaps_old_decode_and_shutdown_reaps_current() {
        let started = Arc::new(AtomicUsize::new(0));
        let reaped = Arc::new(AtomicUsize::new(0));
        let decoder = Arc::new(BlockingDecoder {
            started: Arc::clone(&started),
            reaped: Arc::clone(&reaped),
        });
        let mut worker = AnimationWorker::spawn(
            decoder,
            Arc::new(RecordingPacer::default()),
            Arc::new(AnimationFrameStore::new()),
            8,
        );
        let preview = crate::resolver::PreviewStreamUrl::parse("https://video.invalid/preview")
            .unwrap_or_else(|error| panic!("valid preview: {error}"));
        worker.replace(AnimationRequest::new(
            key(1, "a", CellSize::new(2, 1)),
            preview.clone(),
        ));
        while started.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        worker.replace(AnimationRequest::new(
            key(2, "b", CellSize::new(2, 1)),
            preview,
        ));
        while started.load(Ordering::SeqCst) < 2 || reaped.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }
        worker.shutdown().await;
        assert_eq!(reaped.load(Ordering::SeqCst), 2);
    }

    struct NonCooperativeDecoder {
        started: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    struct DecodeDropGuard(Arc<AtomicUsize>);

    impl Drop for DecodeDropGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl AnimationDecoder for NonCooperativeDecoder {
        async fn decode(
            &self,
            _request: AnimationRequest,
            _output: watch::Sender<Option<AnimationFrameOutput>>,
            _cancel: CancellationToken,
        ) -> Result<(), AnimationError> {
            let _guard = DecodeDropGuard(Arc::clone(&self.dropped));
            self.started.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn replacement_and_shutdown_bound_non_cooperative_decoder_cleanup() {
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut worker = AnimationWorker::spawn(
            Arc::new(NonCooperativeDecoder {
                started: Arc::clone(&started),
                dropped: Arc::clone(&dropped),
            }),
            Arc::new(RecordingPacer::default()),
            Arc::new(AnimationFrameStore::new()),
            8,
        );
        let preview = crate::resolver::PreviewStreamUrl::parse("https://video.invalid/preview")
            .unwrap_or_else(|error| panic!("valid preview: {error}"));
        worker.replace(AnimationRequest::new(
            key(1, "a", CellSize::new(2, 1)),
            preview.clone(),
        ));
        while started.load(Ordering::SeqCst) < 1 {
            tokio::task::yield_now().await;
        }

        worker.replace(AnimationRequest::new(
            key(2, "b", CellSize::new(2, 1)),
            preview,
        ));
        while started.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(Duration::from_millis(500), worker.shutdown())
            .await
            .unwrap_or_else(|_| panic!("shutdown did not bound decoder cleanup"));
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }
}
