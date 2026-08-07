use std::{
    collections::VecDeque,
    future::{self, Future},
    io,
    panic::{self, AssertUnwindSafe, PanicHookInfo, resume_unwind},
    sync::{
        Arc, LazyLock, Mutex, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, EventStream, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::FutureExt as _;
use futures::StreamExt as _;
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use secrecy::SecretString;
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::{JoinHandle, JoinSet},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::{
    app::{
        Action, AppError, AppErrorCategory, AppState, ChartCachePayload, DiagnosticCategory,
        Effect, FavoriteMutation, Generation, OpaqueContinuation, PlayerCommand,
        PodcastProgressCheckpoint, RestoreError, SearchPage, SessionCheckpoint, reduce,
    },
    auth::{AuthService, AuthenticatedProviderFactory, Browser},
    config::Config,
    diagnostics::{
        DependencyChecker, DiagnosticRow, DiagnosticStatus, DoctorReport,
        Platform as DiagnosticPlatform,
    },
    domain::{
        ArtworkUrl, ChartSection, MediaId, MediaItem, MediaKind, PlaybackStatus, RegionCode,
        SearchFilter,
    },
    lyrics::{LyricsDocument, LyricsSourceService},
    notifications::{NotificationWorker, RuntimeNotifier},
    platform::{paths::AppPaths, signals::ShutdownSignals},
    player::supervisor::{PlayerActionStream, PlayerController},
    podcast_rankings::{PodcastRankingError, PodcastRankingSource, match_podcast_recommendation},
    process::{ExecutableLocator, ProcessRunner},
    provider::{
        AuthenticationState, LibraryItem, LibrarySection, MusicProvider, Page, Podcast,
        ProviderError, ProviderResult, SearchItem,
    },
    storage::{
        FavoriteEntry, FavoriteInsertOutcome, HistoryEntry, MetadataCacheEntry, PodcastProgress,
        Storage,
    },
    ui::{
        animation::{AnimationFrameStore, AnimationKey, AnimationRequest, AnimationWorker},
        artwork::{
            ArtworkByteStream, ArtworkFetchError, ArtworkFetcher, ArtworkPresentation,
            ArtworkPresentationStore, CachedArtworkService, CellSize, MAX_ENCODED_BYTES,
            PRODUCTION_ARTWORK_SIZE,
        },
        controller::{UiController, reduce_key, reduce_mouse},
        input::{InputMode, TextEntryContext},
        interaction::{InteractionSnapshot, InteractionStore},
        layout::LayoutMode,
        motion::{MAX_UI_MOTION_FPS, MotionFrame, ProgressChange, ProgressMotion, spinner_index},
        render::{
            RenderEnhancements, RenderModel, ViewportMemory, artwork_presentation_from_stores,
            render_with_model_and_viewports, render_with_model_and_viewports_and_interactions,
        },
        spectrum::{
            MAX_SPECTRUM_BANDS, SpectrumFrameStore, SpectrumKey, SpectrumPresentation,
            SpectrumRequest, SpectrumTarget, SpectrumWorker, effective_spectrum_fps,
        },
        theme::Theme,
    },
};

const DEFAULT_ACTION_CAPACITY: usize = 64;
const INTERNAL_ACTION_CAPACITY: usize = 4;
const EVENT_PENDING_CAPACITY: usize = 16;
const ORDERED_PLAYER_CAPACITY: usize = 8;
const ORDERED_STORAGE_CAPACITY: usize = 16;
const ORDERED_ACCOUNT_CAPACITY: usize = 4;
const SYNC_STORAGE_CAPACITY: usize = 4;
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(3);
const SESSION_DEBOUNCE: Duration = Duration::from_millis(250);
const MAX_PODCAST_MATCH_QUERY_BYTES: usize = 512;
const MAX_PODCAST_MATCH_QUERY_GRAPHEMES: usize = 256;
const MAX_PODCAST_MATCH_CANDIDATES: usize = 64;
const UI_MOTION_INTERVAL: Duration =
    Duration::from_millis(1_000_u64.div_ceil(MAX_UI_MOTION_FPS as u64));

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MotionDemand {
    pub progress: bool,
    pub spinner: bool,
    pub selection: bool,
}

impl MotionDemand {
    #[must_use]
    pub const fn any(self) -> bool {
        self.progress || self.spinner || self.selection
    }

    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        Self {
            progress: self.progress || other.progress,
            spinner: self.spinner || other.spinner,
            selection: self.selection || other.selection,
        }
    }
}

pub struct UiMotionTicker {
    demand: watch::Sender<MotionDemand>,
    redraw: watch::Receiver<u64>,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl UiMotionTicker {
    #[must_use]
    pub fn spawn() -> Self {
        let (demand, mut demand_rx) = watch::channel(MotionDemand::default());
        let (redraw_tx, redraw) = watch::channel(0_u64);
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let task = tokio::spawn(async move {
            loop {
                if !demand_rx.borrow_and_update().any() {
                    tokio::select! {
                        () = task_shutdown.cancelled() => return,
                        changed = demand_rx.changed() => {
                            if changed.is_err() {
                                return;
                            }
                        }
                    }
                    continue;
                }

                tokio::select! {
                    () = task_shutdown.cancelled() => return,
                    changed = demand_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    () = tokio::time::sleep(UI_MOTION_INTERVAL) => {
                        redraw_tx.send_modify(|revision| {
                            *revision = revision.wrapping_add(1);
                        });
                    }
                }
            }
        });
        Self {
            demand,
            redraw,
            shutdown,
            task: Some(task),
        }
    }

    #[must_use]
    pub fn redraw_receiver(&self) -> watch::Receiver<u64> {
        self.redraw.clone()
    }

    pub fn set_demand(&self, demand: MotionDemand) {
        self.demand.send_replace(demand);
    }

    pub async fn shutdown(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("lyrics source is unavailable")]
pub struct RuntimeLyricsError;

impl RuntimeLyricsError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self
    }
}

#[async_trait]
pub trait RuntimeLyrics: Send + Sync {
    async fn load(&self, item: &MediaItem) -> Result<Option<LyricsDocument>, RuntimeLyricsError>;
}

#[async_trait]
impl RuntimeLyrics for LyricsSourceService {
    async fn load(&self, item: &MediaItem) -> Result<Option<LyricsDocument>, RuntimeLyricsError> {
        LyricsSourceService::load(self, item)
            .await
            .map_err(|_| RuntimeLyricsError)
    }
}

#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "runtime events stay ergonomic and their producer queues are independently bounded"
)]
pub enum RuntimeEvent {
    Action(Action),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Redraw,
    Quit,
    Signal,
    Panic,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "the action bus has a small fixed capacity and preserves a direct typed action contract"
)]
pub enum RuntimeMessage {
    Action(Action),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Redraw,
    Quit,
    Panic,
}

#[derive(Clone)]
struct TerminalControlPlane {
    first: CancellationToken,
    emergency: CancellationToken,
    dispatch: CancellationToken,
    requests: Arc<AtomicUsize>,
    panic_requested: Arc<AtomicBool>,
    input: Arc<Mutex<InputAdmissionState>>,
    interrupted: Arc<Mutex<VecDeque<RuntimeMessage>>>,
    handoff: CancellationToken,
}

#[derive(Clone, Debug, Default)]
struct InputAdmissionState {
    acknowledged_mode: InputMode,
    pending_transitions: VecDeque<KeyEvent>,
    projected_mode: ProjectedInputMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectedInputMode {
    Known(InputMode),
    AwaitingAck,
}

impl Default for ProjectedInputMode {
    fn default() -> Self {
        Self::Known(InputMode::default())
    }
}

impl InputAdmissionState {
    fn register(&mut self, key: KeyEvent) {
        if !key_can_transition_input_mode(key) {
            return;
        }
        self.projected_mode = project_input_mode(self.projected_mode, key);
        self.pending_transitions.push_back(key);
    }

    fn unregister_last(&mut self, key: KeyEvent) {
        if !key_can_transition_input_mode(key) {
            return;
        }
        let removed = self
            .pending_transitions
            .iter()
            .rposition(|pending| *pending == key)
            .and_then(|index| self.pending_transitions.remove(index));
        debug_assert_eq!(
            removed,
            Some(key),
            "evicted mode transition must be the latest retained transition"
        );
        self.reproject();
    }

    fn acknowledge(&mut self, key: KeyEvent, mode: InputMode) {
        if key_can_transition_input_mode(key)
            && let Some(index) = self
                .pending_transitions
                .iter()
                .position(|pending| *pending == key)
        {
            debug_assert_eq!(
                index, 0,
                "acknowledged mode transition must preserve event FIFO"
            );
            let _ = self.pending_transitions.remove(index);
        }
        self.acknowledged_mode = mode;
        self.reproject();
    }

    fn reproject(&mut self) {
        self.projected_mode = self.pending_transitions.iter().copied().fold(
            ProjectedInputMode::Known(self.acknowledged_mode),
            project_input_mode,
        );
    }
}

impl TerminalControlPlane {
    fn new(dispatch: CancellationToken) -> Self {
        Self {
            first: CancellationToken::new(),
            emergency: CancellationToken::new(),
            dispatch,
            requests: Arc::new(AtomicUsize::new(0)),
            panic_requested: Arc::new(AtomicBool::new(false)),
            input: Arc::new(Mutex::new(InputAdmissionState::default())),
            interrupted: Arc::new(Mutex::new(VecDeque::with_capacity(EVENT_PENDING_CAPACITY))),
            handoff: CancellationToken::new(),
        }
    }

    fn request(&self, panic: bool) {
        if panic {
            self.panic_requested.store(true, Ordering::Release);
        }
        if self.requests.fetch_add(1, Ordering::AcqRel) == 0 {
            self.dispatch.cancel();
            self.first.cancel();
        } else {
            self.emergency.cancel();
        }
    }

    fn register_pending_mode_change(&self, message: &RuntimeMessage) {
        let RuntimeMessage::Key(key) = message else {
            return;
        };
        self.input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .register(*key);
    }

    fn unregister_pending_mode_change(&self, message: &RuntimeMessage) {
        let RuntimeMessage::Key(key) = message else {
            return;
        };
        self.input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .unregister_last(*key);
    }

    fn acknowledge_key(&self, key: KeyEvent, mode: InputMode) {
        self.input
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .acknowledge(key, mode);
    }

    fn key_requests_exit(&self, key: KeyEvent) -> bool {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return false;
        }
        (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
            || (key.code == KeyCode::Char('q') && key.modifiers.is_empty() && {
                let input = self
                    .input
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                input.projected_mode == ProjectedInputMode::Known(InputMode::Normal)
            })
    }

    fn panic_requested(&self) -> bool {
        self.panic_requested.load(Ordering::Acquire)
    }

    fn publish_pending(&self, pending: &mut VecDeque<RuntimeMessage>) {
        self.interrupted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .append(pending);
        self.handoff.cancel();
    }

    fn publish_pending_and_request(&self, pending: &mut VecDeque<RuntimeMessage>, panic: bool) {
        {
            let mut interrupted = self
                .interrupted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            interrupted.append(pending);
            self.request(panic);
        }
        self.handoff.cancel();
    }

    async fn wait_for_handoff(&self, deadline: Instant) {
        let _ = tokio::time::timeout_at(deadline, self.handoff.cancelled()).await;
    }

    fn take_interrupted(&self) -> VecDeque<RuntimeMessage> {
        std::mem::take(
            &mut *self
                .interrupted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

fn key_can_transition_input_mode(key: KeyEvent) -> bool {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return false;
    }
    match key.code {
        KeyCode::Char('/' | ':') => {
            key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT
        }
        KeyCode::Esc | KeyCode::Enter => key.modifiers.is_empty(),
        _ => false,
    }
}

fn project_input_mode(mode: ProjectedInputMode, key: KeyEvent) -> ProjectedInputMode {
    if !key_can_transition_input_mode(key) {
        return mode;
    }
    match key.code {
        KeyCode::Esc => ProjectedInputMode::Known(InputMode::Normal),
        KeyCode::Enter => match mode {
            ProjectedInputMode::Known(InputMode::TextEntry(TextEntryContext::Palette))
            | ProjectedInputMode::AwaitingAck => ProjectedInputMode::AwaitingAck,
            ProjectedInputMode::Known(
                InputMode::Normal | InputMode::TextEntry(TextEntryContext::Search),
            ) => ProjectedInputMode::Known(InputMode::Normal),
        },
        KeyCode::Char('/') if mode == ProjectedInputMode::Known(InputMode::Normal) => {
            ProjectedInputMode::Known(InputMode::TextEntry(TextEntryContext::Search))
        }
        KeyCode::Char(':') if mode == ProjectedInputMode::Known(InputMode::Normal) => {
            ProjectedInputMode::Known(InputMode::TextEntry(TextEntryContext::Palette))
        }
        _ => mode,
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("runtime action channel is closed")]
pub struct ActionChannelClosed;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ActionSendError {
    #[error("runtime action channel is closed")]
    Closed,
    #[error("runtime action send was cancelled")]
    Cancelled,
}

#[derive(Clone)]
pub struct ActionSender {
    inner: mpsc::Sender<ActionEnvelope>,
    external_capacity: Arc<Semaphore>,
}

struct ActionEnvelope {
    message: RuntimeMessage,
    _external_permit: Option<OwnedSemaphorePermit>,
}

impl ActionSender {
    /// Sends one reducer-facing message, waiting for bounded capacity.
    ///
    /// # Errors
    ///
    /// Returns [`ActionChannelClosed`] after the runtime receiver closes.
    pub async fn send(&self, message: RuntimeMessage) -> Result<(), ActionChannelClosed> {
        let permit = self
            .external_capacity
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| ActionChannelClosed)?;
        self.inner
            .send(ActionEnvelope {
                message,
                _external_permit: Some(permit),
            })
            .await
            .map_err(|_| ActionChannelClosed)
    }

    /// Sends one message unless the owning runtime is shutting down.
    ///
    /// # Errors
    ///
    /// Returns a typed cancellation or closed-channel error without waiting
    /// indefinitely for capacity after shutdown begins.
    pub async fn send_cancellable(
        &self,
        message: RuntimeMessage,
        cancel: &CancellationToken,
    ) -> Result<(), ActionSendError> {
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ActionSendError::Cancelled),
            result = self.external_capacity.clone().acquire_owned() => {
                result.map_err(|_| ActionSendError::Closed)?
            }
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(ActionSendError::Cancelled),
            result = self.inner.send(ActionEnvelope {
                message,
                _external_permit: Some(permit),
            }) => result.map_err(|_| ActionSendError::Closed),
        }
    }

    async fn send_internal_cancellable(
        &self,
        message: RuntimeMessage,
        cancel: &CancellationToken,
    ) -> Result<(), ActionSendError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(ActionSendError::Cancelled),
            result = self.inner.send(ActionEnvelope {
                message,
                _external_permit: None,
            }) => result.map_err(|_| ActionSendError::Closed),
        }
    }
}

pub struct ActionReceiver {
    inner: mpsc::Receiver<ActionEnvelope>,
}

impl ActionReceiver {
    pub async fn recv(&mut self) -> Option<RuntimeMessage> {
        self.inner.recv().await.map(|envelope| envelope.message)
    }
}

#[must_use]
pub fn bounded_action_channel(capacity: usize) -> (ActionSender, ActionReceiver) {
    let capacity = capacity.max(1);
    let (sender, receiver) = mpsc::channel(capacity.saturating_add(INTERNAL_ACTION_CAPACITY));
    let external_capacity = Arc::new(Semaphore::new(capacity));
    (
        ActionSender {
            inner: sender,
            external_capacity,
        },
        ActionReceiver { inner: receiver },
    )
}

#[async_trait]
pub trait EventSource: Send {
    async fn next_event(&mut self) -> Option<RuntimeEvent>;
}

#[allow(
    clippy::large_enum_variant,
    reason = "the transient pump classification avoids a heap allocation for every input event"
)]
enum RuntimeEventDisposition {
    Message(RuntimeMessage),
    Terminal { panic: bool },
}

fn classify_runtime_event(
    event: RuntimeEvent,
    terminal: &TerminalControlPlane,
) -> RuntimeEventDisposition {
    match event {
        RuntimeEvent::Action(action) => {
            RuntimeEventDisposition::Message(RuntimeMessage::Action(action))
        }
        RuntimeEvent::Key(key) if terminal.key_requests_exit(key) => {
            RuntimeEventDisposition::Terminal { panic: false }
        }
        RuntimeEvent::Key(key) => RuntimeEventDisposition::Message(RuntimeMessage::Key(key)),
        RuntimeEvent::Mouse(mouse) => {
            RuntimeEventDisposition::Message(RuntimeMessage::Mouse(mouse))
        }
        RuntimeEvent::Resize(width, height) => {
            RuntimeEventDisposition::Message(RuntimeMessage::Resize(width, height))
        }
        RuntimeEvent::Redraw => RuntimeEventDisposition::Message(RuntimeMessage::Redraw),
        RuntimeEvent::Quit | RuntimeEvent::Signal => {
            RuntimeEventDisposition::Terminal { panic: false }
        }
        RuntimeEvent::Panic => RuntimeEventDisposition::Terminal { panic: true },
    }
}

// The pump retains admitted events in FIFO order. Consecutive pointer movements
// and resize updates coalesce, while redraw hints coalesce globally. At the bound, it may
// discard UI keys, mouse input, and redraw hints so a later terminal control or
// resize remains observable; evicting a mode-changing key also rolls back its
// pending-mode prediction. Clicks and scrolls retain their order whenever
// capacity permits. Injected RuntimeEvent::Action values are lossless and apply
// source backpressure once every retained slot is an action. Production
// player/provider/storage actions enter through the separately bounded action
// bus instead of this event-source queue.
fn enqueue_pending_message(
    pending: &mut VecDeque<RuntimeMessage>,
    message: RuntimeMessage,
    terminal: &TerminalControlPlane,
) {
    if matches!(message, RuntimeMessage::Resize(_, _))
        && matches!(pending.back(), Some(RuntimeMessage::Resize(_, _)))
    {
        let _ = pending.pop_back();
    }
    if matches!(
        message,
        RuntimeMessage::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            ..
        })
    ) && matches!(
        pending.back(),
        Some(RuntimeMessage::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            ..
        }))
    ) {
        let _ = pending.pop_back();
    }
    if matches!(message, RuntimeMessage::Redraw)
        && pending
            .iter()
            .any(|pending| matches!(pending, RuntimeMessage::Redraw))
    {
        return;
    }
    if pending.len() == EVENT_PENDING_CAPACITY {
        if pending_message_is_lossy(&message) {
            if matches!(message, RuntimeMessage::Mouse(MouseEvent { kind, .. }) if kind != MouseEventKind::Moved)
                && let Some(movement) = pending.iter().rposition(|pending| {
                    matches!(
                        pending,
                        RuntimeMessage::Mouse(MouseEvent {
                            kind: MouseEventKind::Moved,
                            ..
                        })
                    )
                })
            {
                let _ = pending.remove(movement);
                pending.push_back(message);
            }
            return;
        }
        let Some(lossy_or_resize) =
            pending
                .iter()
                .rposition(pending_message_is_lossy)
                .or_else(|| {
                    pending
                        .iter()
                        .rposition(|pending| matches!(pending, RuntimeMessage::Resize(_, _)))
                })
        else {
            unreachable!("the event source is polled at capacity only with an evictable slot");
        };
        if let Some(evicted) = pending.remove(lossy_or_resize) {
            terminal.unregister_pending_mode_change(&evicted);
        }
    }
    terminal.register_pending_mode_change(&message);
    pending.push_back(message);
    debug_assert!(pending.len() <= EVENT_PENDING_CAPACITY);
}

fn pending_message_is_lossy(message: &RuntimeMessage) -> bool {
    matches!(
        message,
        RuntimeMessage::Key(_) | RuntimeMessage::Mouse(_) | RuntimeMessage::Redraw
    )
}

fn pending_can_poll_event(pending: &VecDeque<RuntimeMessage>) -> bool {
    pending.len() < EVENT_PENDING_CAPACITY
        || pending.iter().any(|pending| {
            pending_message_is_lossy(pending) || matches!(pending, RuntimeMessage::Resize(_, _))
        })
}

fn absorb_runtime_event(
    event: RuntimeEvent,
    pending: &mut VecDeque<RuntimeMessage>,
    terminal: &TerminalControlPlane,
) -> bool {
    match classify_runtime_event(event, terminal) {
        RuntimeEventDisposition::Message(message) => {
            enqueue_pending_message(pending, message, terminal);
            false
        }
        RuntimeEventDisposition::Terminal { panic } => {
            terminal.publish_pending_and_request(pending, panic);
            true
        }
    }
}

async fn pump_runtime_events<E>(
    mut event_source: E,
    actions: ActionSender,
    shutdown: CancellationToken,
    terminal: TerminalControlPlane,
) where
    E: EventSource,
{
    let mut pending = VecDeque::with_capacity(EVENT_PENDING_CAPACITY);
    let mut controls_only = false;
    loop {
        if controls_only {
            let Some(event) = event_source.next_event().await else {
                return;
            };
            if let RuntimeEventDisposition::Terminal { panic } =
                classify_runtime_event(event, &terminal)
            {
                terminal.request(panic);
            }
            continue;
        }

        if pending.is_empty() {
            let event = tokio::select! {
                biased;
                () = terminal.first.cancelled() => {
                    terminal.publish_pending(&mut pending);
                    controls_only = true;
                    continue;
                }
                () = shutdown.cancelled() => {
                    terminal.publish_pending(&mut pending);
                    return;
                }
                event = event_source.next_event() => event,
            };
            let Some(event) = event else {
                terminal.publish_pending_and_request(&mut pending, false);
                return;
            };
            controls_only = absorb_runtime_event(event, &mut pending, &terminal);
            continue;
        }

        let Some(front) = pending.front().cloned() else {
            continue;
        };
        let admission = actions.send_cancellable(front, &shutdown);
        tokio::pin!(admission);
        let can_poll_event = pending_can_poll_event(&pending);
        tokio::select! {
            biased;
            () = terminal.first.cancelled() => {
                terminal.publish_pending(&mut pending);
                controls_only = true;
            }
            result = &mut admission => match result {
                Ok(()) => {
                    let _ = pending.pop_front();
                }
                Err(ActionSendError::Cancelled | ActionSendError::Closed) => {
                    terminal.publish_pending(&mut pending);
                    return;
                }
            },
            event = event_source.next_event(), if can_poll_event => {
                let Some(event) = event else {
                    terminal.publish_pending_and_request(&mut pending, false);
                    return;
                };
                controls_only = absorb_runtime_event(event, &mut pending, &terminal);
            }
        }
    }
}

pub struct TuiEventSource {
    events: EventStream,
    signals: ShutdownSignals,
}

impl TuiEventSource {
    /// Subscribes to crossterm input and platform shutdown signals.
    ///
    /// # Errors
    ///
    /// Returns an error when process signal handlers cannot be registered.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            events: EventStream::new(),
            signals: ShutdownSignals::new()?,
        })
    }
}

#[async_trait]
impl EventSource for TuiEventSource {
    async fn next_event(&mut self) -> Option<RuntimeEvent> {
        loop {
            tokio::select! {
                signal = self.signals.recv() => {
                    return Some(if signal.is_ok() {
                        RuntimeEvent::Signal
                    } else {
                        RuntimeEvent::Quit
                    });
                }
                event = self.events.next() => {
                    match event {
                        Some(Ok(event)) => {
                            if let Some(event) = runtime_event_from_crossterm(&event) {
                                return Some(event);
                            }
                        }
                        Some(Err(_)) | None => return Some(RuntimeEvent::Quit),
                    }
                }
            }
        }
    }
}

fn runtime_event_from_crossterm(event: &CrosstermEvent) -> Option<RuntimeEvent> {
    match event {
        CrosstermEvent::Key(key) => Some(RuntimeEvent::Key(*key)),
        CrosstermEvent::Mouse(mouse) if supported_mouse_kind(mouse.kind) => {
            Some(RuntimeEvent::Mouse(*mouse))
        }
        CrosstermEvent::Resize(width, height) => Some(RuntimeEvent::Resize(*width, *height)),
        CrosstermEvent::Mouse(_)
        | CrosstermEvent::FocusGained
        | CrosstermEvent::FocusLost
        | CrosstermEvent::Paste(_) => None,
    }
}

const fn supported_mouse_kind(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::Down(crossterm::event::MouseButton::Left)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
    )
}

pub trait Renderer {
    /// Supplies bounded non-durable visualizer presentation configuration.
    fn configure_visualizer_max_fps(&mut self, _max_fps: u8) {}

    /// Returns the current terminal draw area when the renderer exposes one.
    fn area(&self) -> Option<Rect> {
        None
    }

    /// Returns geometry for the latest completed frame, if this renderer publishes it.
    fn interaction_snapshot(&self) -> Option<InteractionSnapshot> {
        None
    }

    /// Invalidates geometry before a terminal resize is reconciled and drawn.
    fn invalidate_interactions(&mut self) {}

    /// Reports transient renderer-owned motion that remains unfinished after
    /// the latest successful frame.
    fn motion_demand(&self) -> MotionDemand {
        MotionDemand::default()
    }

    /// Returns demand measured from the completed frame when the renderer can
    /// report exact visibility. `None` keeps the state-derived fallback used
    /// by non-terminal renderers and tests.
    fn frame_motion_demand(&self) -> Option<MotionDemand> {
        None
    }

    /// Renders one immutable application snapshot.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the target cannot be updated.
    fn render(&mut self, state: &AppState) -> io::Result<()>;

    /// Renders app state together with transient UI-only presentation state.
    ///
    /// Implementations that do not need focus or overlays can retain the
    /// default app-state-only rendering behavior.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the target cannot be updated.
    fn render_with_model(&mut self, state: &AppState, _model: &RenderModel) -> io::Result<()> {
        self.render(state)
    }
}

fn configure_renderer_visualizer(renderer: &mut impl Renderer, max_fps: u8) {
    renderer.configure_visualizer_max_fps(effective_spectrum_fps(max_fps));
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CrosstermTerminalControl;

impl TerminalControl for CrosstermTerminalControl {
    fn enter_alternate_screen(&self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn hide_cursor(&self) -> io::Result<()> {
        execute!(io::stdout(), Hide)
    }

    fn enable_mouse_capture(&self) -> io::Result<()> {
        execute!(io::stdout(), EnableMouseCapture)
    }

    fn enable_raw_mode(&self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn disable_raw_mode(&self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn disable_mouse_capture(&self) -> io::Result<()> {
        execute!(io::stdout(), DisableMouseCapture)
    }

    fn show_cursor(&self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn leave_alternate_screen(&self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen)
    }
}

pub struct TuiRenderer {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    theme: Theme,
    artwork: Option<Arc<ArtworkPresentationStore>>,
    animation: Option<Arc<AnimationFrameStore>>,
    spectrum: Option<Arc<SpectrumFrameStore>>,
    visualizer_max_fps: u8,
    viewports: ViewportMemory,
    interactions: InteractionStore,
}

impl TuiRenderer {
    /// Creates a renderer without changing terminal modes.
    ///
    /// # Errors
    ///
    /// Returns an error if ratatui cannot initialize its stdout backend.
    pub fn new(theme: Theme) -> io::Result<Self> {
        Ok(Self {
            terminal: Terminal::new(CrosstermBackend::new(io::stdout()))?,
            theme,
            artwork: None,
            animation: None,
            spectrum: None,
            visualizer_max_fps: 15,
            viewports: ViewportMemory::default(),
            interactions: InteractionStore::default(),
        })
    }

    #[must_use]
    pub fn with_artwork_store(mut self, artwork: Arc<ArtworkPresentationStore>) -> Self {
        self.artwork = Some(artwork);
        self
    }

    #[must_use]
    pub fn with_animation_store(mut self, animation: Arc<AnimationFrameStore>) -> Self {
        self.animation = Some(animation);
        self
    }

    #[must_use]
    pub fn with_spectrum_store(mut self, spectrum: Arc<SpectrumFrameStore>) -> Self {
        self.spectrum = Some(spectrum);
        self
    }

    fn artwork_presentation(
        &self,
        state: &AppState,
        layout: LayoutMode,
    ) -> Option<ArtworkPresentation> {
        artwork_presentation_from_stores(
            state,
            self.artwork.as_deref(),
            self.animation.as_deref(),
            PRODUCTION_ARTWORK_SIZE,
            layout,
        )
    }

    fn spectrum_presentation(
        &self,
        state: &AppState,
        layout: LayoutMode,
        width: u16,
    ) -> Option<SpectrumPresentation> {
        let store = self.spectrum.as_deref()?;
        if !state.visualizer_enabled() || layout == LayoutMode::Tiny {
            return None;
        }
        let rows = if layout == LayoutMode::Wide { 3 } else { 1 };
        let maximum_bands = u16::try_from(MAX_SPECTRUM_BANDS).ok()?;
        let target = SpectrumTarget::new(width.clamp(1, maximum_bands), rows)?;
        Some(
            spectrum_key_for_state(state, target)
                .map_or_else(SpectrumPresentation::quiet, |key| store.presentation(&key)),
        )
    }

    fn draw_frame(
        &mut self,
        state: &AppState,
        model: &RenderModel,
        enhancements: RenderEnhancements<'_>,
    ) -> io::Result<()> {
        let terminal = &mut self.terminal;
        let theme = &self.theme;
        let viewports = &mut self.viewports;
        draw_interaction_frame(&mut self.interactions, |interactions| {
            terminal.draw(|frame| {
                if let Some(interactions) = interactions {
                    render_with_model_and_viewports_and_interactions(
                        frame,
                        state,
                        theme,
                        model,
                        enhancements,
                        viewports,
                        interactions,
                    );
                } else {
                    render_with_model_and_viewports(
                        frame,
                        state,
                        theme,
                        model,
                        enhancements,
                        viewports,
                    );
                }
            })
        })
        .map(|_| ())
    }
}

fn draw_interaction_frame<T, E>(
    interactions: &mut InteractionStore,
    draw: impl FnOnce(Option<&mut crate::ui::interaction::InteractionMap>) -> Result<T, E>,
) -> Result<T, E> {
    let mut frame_map = interactions.begin_frame();
    let result = draw(frame_map.as_mut());
    if result.is_ok()
        && let Some(frame_map) = frame_map
    {
        let published = interactions.publish(frame_map);
        debug_assert!(
            published,
            "completed frame must retain its reserved revision"
        );
    }
    result
}

impl Renderer for TuiRenderer {
    fn configure_visualizer_max_fps(&mut self, max_fps: u8) {
        self.visualizer_max_fps = effective_spectrum_fps(max_fps);
    }

    fn area(&self) -> Option<Rect> {
        self.terminal
            .size()
            .ok()
            .map(|size| Rect::new(0, 0, size.width, size.height))
    }

    fn interaction_snapshot(&self) -> Option<InteractionSnapshot> {
        self.interactions.latest().cloned()
    }

    fn invalidate_interactions(&mut self) {
        self.interactions.invalidate();
    }

    fn frame_motion_demand(&self) -> Option<MotionDemand> {
        Some(MotionDemand {
            progress: self.viewports.progress_motion_visible(),
            spinner: self.viewports.spinner_motion_visible(),
            selection: self.viewports.selection_transitioning(),
        })
    }

    fn render(&mut self, state: &AppState) -> io::Result<()> {
        let model = RenderModel::default();
        let area = self.area();
        let layout = area.map_or(LayoutMode::Tiny, LayoutMode::for_area);
        let spectrum = self.spectrum_presentation(state, layout, area.map_or(0, |area| area.width));
        let visualizer_max_fps = self.visualizer_max_fps;
        if let Some(artwork) = self.artwork_presentation(state, layout) {
            self.draw_frame(
                state,
                &model,
                RenderEnhancements::new(Some(&artwork), spectrum.as_ref(), visualizer_max_fps),
            )
        } else {
            self.draw_frame(
                state,
                &model,
                RenderEnhancements::new(None, spectrum.as_ref(), visualizer_max_fps),
            )
        }
    }

    fn render_with_model(&mut self, state: &AppState, model: &RenderModel) -> io::Result<()> {
        let area = self.area();
        let layout = area.map_or(LayoutMode::Tiny, LayoutMode::for_area);
        let spectrum = self.spectrum_presentation(state, layout, area.map_or(0, |area| area.width));
        let visualizer_max_fps = self.visualizer_max_fps;
        if let Some(artwork) = self.artwork_presentation(state, layout) {
            self.draw_frame(
                state,
                model,
                RenderEnhancements::new(Some(&artwork), spectrum.as_ref(), visualizer_max_fps),
            )
        } else {
            self.draw_frame(
                state,
                model,
                RenderEnhancements::new(None, spectrum.as_ref(), visualizer_max_fps),
            )
        }
    }
}

/// Minimal terminal operations owned by the runtime's process boundary.
#[allow(
    clippy::missing_errors_doc,
    reason = "each method is one direct terminal I/O operation with the same error contract"
)]
pub trait TerminalControl: Send + Sync {
    fn enter_alternate_screen(&self) -> io::Result<()>;
    fn hide_cursor(&self) -> io::Result<()>;
    fn enable_mouse_capture(&self) -> io::Result<()>;
    fn enable_raw_mode(&self) -> io::Result<()>;
    fn disable_raw_mode(&self) -> io::Result<()>;
    fn disable_mouse_capture(&self) -> io::Result<()>;
    fn show_cursor(&self) -> io::Result<()>;
    fn leave_alternate_screen(&self) -> io::Result<()>;
}

/// Restores every terminal mode it successfully acquired, including on unwind.
pub struct TerminalGuard {
    terminal: Arc<dyn TerminalControl>,
    modes: Arc<TerminalModes>,
}

#[derive(Default)]
struct TerminalModes {
    alternate_screen: AtomicBool,
    cursor_hidden: AtomicBool,
    mouse_capture: AtomicBool,
    raw_mode: AtomicBool,
}

#[derive(Clone)]
struct TerminalRestorer {
    terminal: Arc<dyn TerminalControl>,
    modes: Arc<TerminalModes>,
}

impl TerminalGuard {
    /// Acquires terminal modes in display-to-input order.
    ///
    /// # Errors
    ///
    /// If one operation fails, all earlier operations are restored before the
    /// error is returned.
    pub fn acquire(terminal: Arc<dyn TerminalControl>) -> io::Result<Self> {
        let guard = Self {
            terminal,
            modes: Arc::new(TerminalModes::default()),
        };
        guard.terminal.enter_alternate_screen()?;
        guard.modes.alternate_screen.store(true, Ordering::Release);
        guard.terminal.hide_cursor()?;
        guard.modes.cursor_hidden.store(true, Ordering::Release);
        guard.terminal.enable_mouse_capture()?;
        guard.modes.mouse_capture.store(true, Ordering::Release);
        guard.terminal.enable_raw_mode()?;
        guard.modes.raw_mode.store(true, Ordering::Release);
        Ok(guard)
    }

    /// Restores acquired modes in strict reverse order.
    ///
    /// All restoration steps are attempted even if an earlier one fails.
    ///
    /// # Errors
    ///
    /// Returns the first terminal restoration error, if any.
    pub fn restore(&mut self) -> io::Result<()> {
        self.restorer().restore()
    }

    fn restorer(&self) -> TerminalRestorer {
        TerminalRestorer {
            terminal: Arc::clone(&self.terminal),
            modes: Arc::clone(&self.modes),
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl TerminalRestorer {
    fn restore(&self) -> io::Result<()> {
        let mut first_error = None;
        if self.modes.raw_mode.swap(false, Ordering::AcqRel) {
            record_first_error(&mut first_error, self.terminal.disable_raw_mode());
        }
        if self.modes.mouse_capture.swap(false, Ordering::AcqRel) {
            record_first_error(&mut first_error, self.terminal.disable_mouse_capture());
        }
        if self.modes.cursor_hidden.swap(false, Ordering::AcqRel) {
            record_first_error(&mut first_error, self.terminal.show_cursor());
        }
        if self.modes.alternate_screen.swap(false, Ordering::AcqRel) {
            record_first_error(&mut first_error, self.terminal.leave_alternate_screen());
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn record_first_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result
        && first_error.is_none()
    {
        *first_error = Some(error);
    }
}

type SharedPanicHook = dyn for<'a> Fn(&PanicHookInfo<'a>) + Send + Sync + 'static;

static PANIC_HOOK_LOCK: LazyLock<Arc<tokio::sync::Mutex<()>>> =
    LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(())));

struct RuntimePanicHook {
    previous: Arc<SharedPanicHook>,
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl RuntimePanicHook {
    async fn install(restorer: TerminalRestorer) -> Self {
        let lock = Arc::clone(&PANIC_HOOK_LOCK).lock_owned().await;
        let previous: Arc<SharedPanicHook> = Arc::from(panic::take_hook());
        let delegate = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            let _ = restorer.restore();
            delegate(info);
        }));
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for RuntimePanicHook {
    fn drop(&mut self) {
        let _ = panic::take_hook();
        let previous = Arc::clone(&self.previous);
        panic::set_hook(Box::new(move |info| previous(info)));
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("runtime storage operation failed")]
pub struct RuntimeStorageError;

impl From<crate::storage::StorageError> for RuntimeStorageError {
    fn from(_: crate::storage::StorageError) -> Self {
        Self
    }
}

#[async_trait]
pub trait RuntimeStorage: Send + Sync {
    /// Loads the durable session snapshot, if one exists.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error when the operation cannot complete.
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError>;

    /// Saves one durable session checkpoint.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error when the operation cannot complete.
    async fn save_session(
        &self,
        _checkpoint: SessionCheckpoint,
        _updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Err(RuntimeStorageError)
    }

    /// Saves one podcast progress observation without changing its epoch.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error when the operation cannot complete.
    async fn save_podcast_progress(
        &self,
        _progress: PodcastProgress,
    ) -> Result<(), RuntimeStorageError> {
        Err(RuntimeStorageError)
    }

    /// Loads one podcast progress row unchanged.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error when the operation cannot complete.
    async fn load_podcast_progress(
        &self,
        _video_id: String,
    ) -> Result<Option<PodcastProgress>, RuntimeStorageError> {
        Err(RuntimeStorageError)
    }

    /// Loads one chart cache row.
    ///
    /// # Errors
    ///
    /// Returns a redacted storage error when the operation cannot complete.
    async fn load_chart_cache(
        &self,
        _key: &str,
    ) -> Result<Option<MetadataCacheEntry>, RuntimeStorageError> {
        Ok(None)
    }

    async fn store_chart_cache(
        &self,
        _key: String,
        _payload: String,
        _expires_at: i64,
        _stored_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Err(RuntimeStorageError)
    }

    async fn record_history(
        &self,
        _item: MediaItem,
        _played_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        Err(RuntimeStorageError)
    }

    async fn load_history(&self, _limit: usize) -> Result<Vec<HistoryEntry>, RuntimeStorageError> {
        Err(RuntimeStorageError)
    }

    async fn load_favorites(&self) -> Result<Vec<FavoriteEntry>, RuntimeStorageError> {
        Err(RuntimeStorageError)
    }

    async fn add_favorite(
        &self,
        _item: MediaItem,
        _favorited_at: i64,
    ) -> Result<FavoriteInsertOutcome, RuntimeStorageError> {
        Err(RuntimeStorageError)
    }

    async fn remove_favorite(&self, _id: MediaId) -> Result<bool, RuntimeStorageError> {
        Err(RuntimeStorageError)
    }
}

pub struct FifoStorage {
    commands: mpsc::Sender<StorageCommand>,
}

impl FifoStorage {
    /// Starts one dedicated FIFO owner for a synchronous storage backend.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the owner thread cannot be created.
    pub fn spawn(storage: Box<dyn Storage>) -> io::Result<Self> {
        let (commands, receiver) = mpsc::channel(SYNC_STORAGE_CAPACITY);
        thread::Builder::new()
            .name("ytermusic-storage".to_owned())
            .spawn(move || storage_owner_loop(storage, receiver))?;
        Ok(Self { commands })
    }
}

enum StorageCommand {
    LoadSession(oneshot::Sender<Result<Option<SessionCheckpoint>, RuntimeStorageError>>),
    SaveSession {
        checkpoint: SessionCheckpoint,
        updated_at: i64,
        completed: oneshot::Sender<Result<(), RuntimeStorageError>>,
    },
    SavePodcastProgress {
        progress: PodcastProgress,
        completed: oneshot::Sender<Result<(), RuntimeStorageError>>,
    },
    LoadPodcastProgress {
        video_id: String,
        completed: oneshot::Sender<Result<Option<PodcastProgress>, RuntimeStorageError>>,
    },
    LoadChartCache {
        key: String,
        completed: oneshot::Sender<Result<Option<MetadataCacheEntry>, RuntimeStorageError>>,
    },
    StoreChartCache {
        key: String,
        payload: String,
        expires_at: i64,
        stored_at: i64,
        completed: oneshot::Sender<Result<(), RuntimeStorageError>>,
    },
    RecordHistory {
        item: MediaItem,
        played_at: i64,
        completed: oneshot::Sender<Result<(), RuntimeStorageError>>,
    },
    LoadHistory {
        limit: usize,
        completed: oneshot::Sender<Result<Vec<HistoryEntry>, RuntimeStorageError>>,
    },
    LoadFavorites {
        completed: oneshot::Sender<Result<Vec<FavoriteEntry>, RuntimeStorageError>>,
    },
    AddFavorite {
        item: MediaItem,
        favorited_at: i64,
        completed: oneshot::Sender<Result<FavoriteInsertOutcome, RuntimeStorageError>>,
    },
    RemoveFavorite {
        id: MediaId,
        completed: oneshot::Sender<Result<bool, RuntimeStorageError>>,
    },
}

fn storage_owner_loop(mut storage: Box<dyn Storage>, mut commands: mpsc::Receiver<StorageCommand>) {
    while let Some(command) = commands.blocking_recv() {
        match command {
            StorageCommand::LoadSession(completed) => {
                let _ = completed.send(storage.load_session().map_err(Into::into));
            }
            StorageCommand::SaveSession {
                checkpoint,
                updated_at,
                completed,
            } => {
                let _ = completed.send(
                    storage
                        .save_session(&checkpoint, updated_at)
                        .map_err(Into::into),
                );
            }
            StorageCommand::SavePodcastProgress {
                progress,
                completed,
            } => {
                let _ =
                    completed.send(storage.save_podcast_progress(&progress).map_err(Into::into));
            }
            StorageCommand::LoadPodcastProgress {
                video_id,
                completed,
            } => {
                let _ =
                    completed.send(storage.load_podcast_progress(&video_id).map_err(Into::into));
            }
            StorageCommand::LoadChartCache { key, completed } => {
                let _ = completed.send(storage.get_metadata_entry(&key).map_err(Into::into));
            }
            StorageCommand::StoreChartCache {
                key,
                payload,
                expires_at,
                stored_at,
                completed,
            } => {
                let _ = completed.send(
                    storage
                        .put_metadata(&key, &payload, expires_at, stored_at)
                        .map_err(Into::into),
                );
            }
            StorageCommand::RecordHistory {
                item,
                played_at,
                completed,
            } => {
                let _ =
                    completed.send(storage.record_history(&item, played_at).map_err(Into::into));
            }
            StorageCommand::LoadHistory { limit, completed } => {
                let _ = completed.send(storage.recent_history(limit).map_err(Into::into));
            }
            StorageCommand::LoadFavorites { completed } => {
                let _ = completed.send(storage.load_favorites().map_err(Into::into));
            }
            StorageCommand::AddFavorite {
                item,
                favorited_at,
                completed,
            } => {
                let _ = completed.send(
                    storage
                        .add_favorite(&item, favorited_at)
                        .map_err(Into::into),
                );
            }
            StorageCommand::RemoveFavorite { id, completed } => {
                let _ = completed.send(storage.remove_favorite(&id).map_err(Into::into));
            }
        }
    }
    let _ = &mut storage;
}

#[async_trait]
impl RuntimeStorage for FifoStorage {
    async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::LoadSession(completed))
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn save_session(
        &self,
        checkpoint: SessionCheckpoint,
        updated_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::SaveSession {
                checkpoint,
                updated_at,
                completed,
            })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn save_podcast_progress(
        &self,
        progress: PodcastProgress,
    ) -> Result<(), RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::SavePodcastProgress {
                progress,
                completed,
            })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn load_podcast_progress(
        &self,
        video_id: String,
    ) -> Result<Option<PodcastProgress>, RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::LoadPodcastProgress {
                video_id,
                completed,
            })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn load_chart_cache(
        &self,
        key: &str,
    ) -> Result<Option<MetadataCacheEntry>, RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::LoadChartCache {
                key: key.to_owned(),
                completed,
            })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn store_chart_cache(
        &self,
        key: String,
        payload: String,
        expires_at: i64,
        stored_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::StoreChartCache {
                key,
                payload,
                expires_at,
                stored_at,
                completed,
            })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn record_history(
        &self,
        item: MediaItem,
        played_at: i64,
    ) -> Result<(), RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::RecordHistory {
                item,
                played_at,
                completed,
            })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn load_history(&self, limit: usize) -> Result<Vec<HistoryEntry>, RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::LoadHistory { limit, completed })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn load_favorites(&self) -> Result<Vec<FavoriteEntry>, RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::LoadFavorites { completed })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn add_favorite(
        &self,
        item: MediaItem,
        favorited_at: i64,
    ) -> Result<FavoriteInsertOutcome, RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::AddFavorite {
                item,
                favorited_at,
                completed,
            })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }

    async fn remove_favorite(&self, id: MediaId) -> Result<bool, RuntimeStorageError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(StorageCommand::RemoveFavorite { id, completed })
            .await
            .map_err(|_| RuntimeStorageError)?;
        result.await.map_err(|_| RuntimeStorageError)?
    }
}

pub trait RuntimeClock: Send + Sync {
    fn now_millis(&self) -> i64;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRuntimeClock;

impl RuntimeClock for SystemRuntimeClock {
    fn now_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("runtime player operation failed")]
pub struct RuntimePlayerError;

#[async_trait]
pub trait RuntimePlayer: Send + Sync {
    /// Starts one generation-tagged playback attempt.
    ///
    /// # Errors
    ///
    /// Returns a redacted player error when the command cannot be accepted.
    async fn play(
        &self,
        generation: Generation,
        item: MediaItem,
        start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError>;

    async fn pause(&self) -> Result<(), RuntimePlayerError>;
    async fn resume(&self) -> Result<(), RuntimePlayerError>;
    async fn set_volume(&self, volume: u8) -> Result<(), RuntimePlayerError>;

    async fn seek_relative(&self, _seconds: i64) -> Result<(), RuntimePlayerError> {
        Err(RuntimePlayerError)
    }

    async fn shutdown(&self) -> Result<(), RuntimePlayerError> {
        Ok(())
    }

    fn abort(&self) {}
}

#[async_trait]
pub trait RuntimePlayerActions: Send {
    async fn next_action(&mut self) -> Option<Action>;
}

#[async_trait]
impl RuntimePlayer for PlayerController {
    async fn play(
        &self,
        generation: Generation,
        item: MediaItem,
        start_ms: Option<u64>,
    ) -> Result<(), RuntimePlayerError> {
        PlayerController::play(self, generation, item, start_ms)
            .await
            .map_err(|_| RuntimePlayerError)
    }

    async fn pause(&self) -> Result<(), RuntimePlayerError> {
        PlayerController::pause(self)
            .await
            .map_err(|_| RuntimePlayerError)
    }

    async fn resume(&self) -> Result<(), RuntimePlayerError> {
        PlayerController::resume(self)
            .await
            .map_err(|_| RuntimePlayerError)
    }

    async fn set_volume(&self, volume: u8) -> Result<(), RuntimePlayerError> {
        PlayerController::set_volume(self, volume)
            .await
            .map_err(|_| RuntimePlayerError)
    }

    async fn seek_relative(&self, seconds: i64) -> Result<(), RuntimePlayerError> {
        PlayerController::seek_relative(self, seconds)
            .await
            .map_err(|_| RuntimePlayerError)
    }

    async fn shutdown(&self) -> Result<(), RuntimePlayerError> {
        PlayerController::shutdown(self)
            .await
            .map_err(|_| RuntimePlayerError)
    }

    fn abort(&self) {
        PlayerController::abort(self);
    }
}

#[async_trait]
impl RuntimePlayerActions for PlayerActionStream {
    async fn next_action(&mut self) -> Option<Action> {
        PlayerActionStream::next_action(self).await
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("runtime service operation failed")]
pub struct RuntimeServiceError;

#[async_trait]
pub trait RuntimeAccount: Send + Sync {
    async fn connect(&self, browser: Browser) -> Result<AuthenticationState, RuntimeServiceError>;

    async fn shutdown(&self) {}
}

#[async_trait]
pub trait RuntimeCredentialImporter: Send + Sync {
    async fn prepare(&self, browser: Browser) -> Result<SecretString, RuntimeServiceError>;
    async fn commit(&self, credential: SecretString) -> Result<(), RuntimeServiceError>;
}

pub struct SharedMusicProvider {
    provider: RwLock<Arc<dyn MusicProvider>>,
}

impl SharedMusicProvider {
    #[must_use]
    pub fn new(provider: Arc<dyn MusicProvider>) -> Self {
        Self {
            provider: RwLock::new(provider),
        }
    }

    fn snapshot(&self) -> Arc<dyn MusicProvider> {
        Arc::clone(
            &self
                .provider
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn replace(&self, provider: Arc<dyn MusicProvider>) {
        *self
            .provider
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = provider;
    }
}

#[async_trait]
impl MusicProvider for SharedMusicProvider {
    async fn search(&self, query: &str, filter: SearchFilter) -> ProviderResult<Page<SearchItem>> {
        self.snapshot().search(query, filter).await
    }

    async fn search_more(
        &self,
        query: &str,
        filter: SearchFilter,
        continuation: &str,
    ) -> ProviderResult<Page<SearchItem>> {
        self.snapshot()
            .search_more(query, filter, continuation)
            .await
    }

    async fn charts(&self, region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        self.snapshot().charts(region).await
    }

    async fn playlist(&self, id: &str) -> ProviderResult<Vec<MediaItem>> {
        self.snapshot().playlist(id).await
    }

    async fn podcast(&self, id: &str) -> ProviderResult<Podcast> {
        self.snapshot().podcast(id).await
    }

    async fn radio(&self, seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        self.snapshot().radio(seed).await
    }

    async fn lyrics(&self, id: &MediaId) -> ProviderResult<crate::provider::PlainLyrics> {
        self.snapshot().lyrics(id).await
    }

    async fn library(&self, section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        self.snapshot().library(section).await
    }

    async fn library_more(
        &self,
        section: LibrarySection,
        continuation: &str,
    ) -> ProviderResult<Page<LibraryItem>> {
        self.snapshot().library_more(section, continuation).await
    }

    fn authentication(&self) -> AuthenticationState {
        self.snapshot().authentication()
    }
}

struct AuthServiceCredentialImporter {
    auth: Arc<AuthService>,
}

#[async_trait]
impl RuntimeCredentialImporter for AuthServiceCredentialImporter {
    async fn prepare(&self, browser: Browser) -> Result<SecretString, RuntimeServiceError> {
        self.auth
            .prepare_browser_cookie(browser)
            .await
            .map_err(|_| RuntimeServiceError)
    }

    async fn commit(&self, credential: SecretString) -> Result<(), RuntimeServiceError> {
        self.auth
            .commit_browser_cookie(credential)
            .await
            .map_err(|_| RuntimeServiceError)
    }
}

pub struct RuntimeAccountService {
    importer: Arc<dyn RuntimeCredentialImporter>,
    factory: Arc<dyn AuthenticatedProviderFactory>,
    provider: Arc<SharedMusicProvider>,
    imports: Arc<tokio::sync::Mutex<()>>,
    critical_tasks: tokio::sync::Mutex<JoinSet<()>>,
}

impl RuntimeAccountService {
    #[must_use]
    pub fn new(
        auth: Arc<AuthService>,
        factory: Arc<dyn AuthenticatedProviderFactory>,
        provider: Arc<SharedMusicProvider>,
    ) -> Self {
        Self {
            importer: Arc::new(AuthServiceCredentialImporter { auth }),
            factory,
            provider,
            imports: Arc::new(tokio::sync::Mutex::new(())),
            critical_tasks: tokio::sync::Mutex::new(JoinSet::new()),
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub fn with_importer(
        importer: Arc<dyn RuntimeCredentialImporter>,
        factory: Arc<dyn AuthenticatedProviderFactory>,
        provider: Arc<SharedMusicProvider>,
    ) -> Self {
        Self {
            importer,
            factory,
            provider,
            imports: Arc::new(tokio::sync::Mutex::new(())),
            critical_tasks: tokio::sync::Mutex::new(JoinSet::new()),
        }
    }
}

#[async_trait]
impl RuntimeAccount for RuntimeAccountService {
    async fn connect(&self, browser: Browser) -> Result<AuthenticationState, RuntimeServiceError> {
        let import = self.imports.clone().lock_owned().await;
        {
            let mut tasks = self.critical_tasks.lock().await;
            while tasks.try_join_next().is_some() {}
        }
        let credential = self.importer.prepare(browser).await?;
        let replacement = self
            .factory
            .create(&credential)
            .await
            .map_err(|_| RuntimeServiceError)?;
        if replacement.authentication() != AuthenticationState::Authenticated {
            return Err(RuntimeServiceError);
        }
        let importer = Arc::clone(&self.importer);
        let provider = Arc::clone(&self.provider);
        let (completed, result) = oneshot::channel();
        self.critical_tasks.lock().await.spawn(async move {
            let result = importer.commit(credential).await.map(|()| {
                provider.replace(replacement);
                AuthenticationState::Authenticated
            });
            drop(import);
            let _ = completed.send(result);
        });
        result.await.map_err(|_| RuntimeServiceError)?
    }

    async fn shutdown(&self) {
        let mut tasks = self.critical_tasks.lock().await;
        while tasks.join_next().await.is_some() {}
    }
}

#[async_trait]
pub trait RuntimeDependencies: Send + Sync {
    async fn check(&self) -> DoctorReport;
}

#[async_trait]
pub trait RuntimeArtwork: Send + Sync {
    fn request(&self, _generation: Generation, _url: &ArtworkUrl) {}

    fn clear(&self) {}

    async fn fetch(
        &self,
        generation: Generation,
        url: ArtworkUrl,
    ) -> Result<(), RuntimeServiceError>;
}

pub struct RuntimeArtworkService<F> {
    service: tokio::sync::Mutex<CachedArtworkService<F>>,
    store: Arc<ArtworkPresentationStore>,
    size: CellSize,
    capability: crate::ui::theme::ColorCapability,
}

pub struct ArtworkRuntimeComponents<F> {
    artwork: Arc<RuntimeArtworkService<F>>,
    store: Arc<ArtworkPresentationStore>,
}

impl<F> ArtworkRuntimeComponents<F>
where
    F: ArtworkFetcher + Send + Sync + 'static,
{
    #[must_use]
    pub fn new(
        fetcher: F,
        capacity: usize,
        size: CellSize,
        capability: crate::ui::theme::ColorCapability,
    ) -> Self {
        let store = Arc::new(ArtworkPresentationStore::new());
        let artwork = Arc::new(RuntimeArtworkService::new(
            fetcher,
            capacity,
            store.clone(),
            size,
            capability,
        ));
        Self { artwork, store }
    }

    #[must_use]
    pub fn runtime_artwork(&self) -> Arc<dyn RuntimeArtwork> {
        self.artwork.clone()
    }

    #[must_use]
    pub fn presentation_store(&self) -> Arc<ArtworkPresentationStore> {
        self.store.clone()
    }
}

impl<F> RuntimeArtworkService<F>
where
    F: ArtworkFetcher,
{
    #[must_use]
    pub fn new(
        fetcher: F,
        capacity: usize,
        store: Arc<ArtworkPresentationStore>,
        size: CellSize,
        capability: crate::ui::theme::ColorCapability,
    ) -> Self {
        Self {
            service: tokio::sync::Mutex::new(CachedArtworkService::new(fetcher, capacity)),
            store,
            size,
            capability,
        }
    }
}

#[async_trait]
impl<F> RuntimeArtwork for RuntimeArtworkService<F>
where
    F: ArtworkFetcher + Send + Sync,
{
    fn request(&self, generation: Generation, url: &ArtworkUrl) {
        self.store.request(generation, url);
    }

    fn clear(&self) {
        self.store.clear();
    }

    async fn fetch(
        &self,
        generation: Generation,
        url: ArtworkUrl,
    ) -> Result<(), RuntimeServiceError> {
        let presentation = self
            .service
            .lock()
            .await
            .load(url.as_url(), self.size, self.capability)
            .await;
        let success = presentation.is_grid();
        let _ = self.store.publish(generation, &url, presentation);
        if success {
            Ok(())
        } else {
            Err(RuntimeServiceError)
        }
    }
}

pub struct SystemRuntimeDependencies {
    locator: Arc<dyn ExecutableLocator>,
    runner: Arc<dyn ProcessRunner>,
    platform: DiagnosticPlatform,
}

impl SystemRuntimeDependencies {
    #[must_use]
    pub const fn new(
        locator: Arc<dyn ExecutableLocator>,
        runner: Arc<dyn ProcessRunner>,
        platform: DiagnosticPlatform,
    ) -> Self {
        Self {
            locator,
            runner,
            platform,
        }
    }
}

#[async_trait]
impl RuntimeDependencies for SystemRuntimeDependencies {
    async fn check(&self) -> DoctorReport {
        DependencyChecker::new(self.locator.as_ref(), self.runner.as_ref(), self.platform)
            .check()
            .await
    }
}

pub struct HttpArtworkFetcher {
    client: reqwest::Client,
}

impl HttpArtworkFetcher {
    /// Builds a secret-safe HTTP artwork stream boundary.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTPS client cannot be configured.
    pub fn new() -> Result<Self, RuntimeServiceError> {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(10))
            .build()
            .map(|client| Self { client })
            .map_err(|_| RuntimeServiceError)
    }
}

#[async_trait]
impl ArtworkFetcher for HttpArtworkFetcher {
    async fn fetch(&self, url: &url::Url) -> Result<ArtworkByteStream, ArtworkFetchError> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|_| ArtworkFetchError::unavailable())?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_ENCODED_BYTES as u64)
        {
            return Err(ArtworkFetchError::unavailable());
        }
        Ok(Box::pin(response.bytes_stream().map(|chunk| {
            chunk.map_err(|_| ArtworkFetchError::unavailable())
        })))
    }
}

pub struct RuntimeServices {
    storage: Arc<dyn RuntimeStorage>,
    provider: Option<Arc<dyn MusicProvider>>,
    podcast_rankings: Option<Arc<dyn PodcastRankingSource>>,
    player: Option<Arc<dyn RuntimePlayer>>,
    player_actions: Option<Box<dyn RuntimePlayerActions>>,
    account: Option<Arc<dyn RuntimeAccount>>,
    dependencies: Option<Arc<dyn RuntimeDependencies>>,
    artwork: Option<Arc<dyn RuntimeArtwork>>,
    lyrics: Option<Arc<dyn RuntimeLyrics>>,
    notifier: Option<Arc<dyn RuntimeNotifier>>,
    animation: Option<AnimationWorker>,
    spectrum: Option<SpectrumWorker>,
    terminal: Option<Arc<dyn TerminalControl>>,
    clock: Arc<dyn RuntimeClock>,
    initial_actions: Vec<Action>,
    action_capacity: usize,
    shutdown_timeout: Duration,
}

#[must_use]
pub fn startup_actions(
    authentication: AuthenticationState,
    dependencies: DoctorReport,
) -> Vec<Action> {
    vec![
        Action::AuthenticationChanged(authentication),
        Action::DependencyReportLoaded(dependencies),
        Action::FavoritesRequested,
    ]
}

impl RuntimeServices {
    #[must_use]
    pub fn new(storage: Arc<dyn RuntimeStorage>) -> Self {
        Self {
            storage,
            provider: None,
            podcast_rankings: None,
            player: None,
            player_actions: None,
            account: None,
            dependencies: None,
            artwork: None,
            lyrics: None,
            notifier: None,
            animation: None,
            spectrum: None,
            terminal: None,
            clock: Arc::new(SystemRuntimeClock),
            initial_actions: Vec::new(),
            action_capacity: DEFAULT_ACTION_CAPACITY,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        }
    }

    /// Adds the standard production startup actions in their required order.
    #[must_use]
    pub(crate) fn with_startup_actions(
        mut self,
        authentication: AuthenticationState,
        dependencies: DoctorReport,
    ) -> Self {
        self.initial_actions
            .extend(startup_actions(authentication, dependencies));
        self
    }

    #[must_use]
    pub fn with_provider(mut self, provider: Arc<dyn MusicProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_podcast_rankings(mut self, source: Arc<dyn PodcastRankingSource>) -> Self {
        self.podcast_rankings = Some(source);
        self
    }

    #[must_use]
    pub fn with_account_provider(
        mut self,
        provider: Arc<SharedMusicProvider>,
        account: Arc<dyn RuntimeAccount>,
    ) -> Self {
        self.provider = Some(provider);
        self.account = Some(account);
        self
    }

    #[must_use]
    pub fn with_player(mut self, player: Arc<dyn RuntimePlayer>) -> Self {
        self.player = Some(player);
        self
    }

    #[must_use]
    pub fn with_player_actions(mut self, player_actions: Box<dyn RuntimePlayerActions>) -> Self {
        self.player_actions = Some(player_actions);
        self
    }

    #[must_use]
    pub fn with_account(mut self, account: Arc<dyn RuntimeAccount>) -> Self {
        self.account = Some(account);
        self
    }

    #[must_use]
    pub const fn account_configured(&self) -> bool {
        self.account.is_some()
    }

    #[must_use]
    pub fn with_dependencies(mut self, dependencies: Arc<dyn RuntimeDependencies>) -> Self {
        self.dependencies = Some(dependencies);
        self
    }

    #[must_use]
    pub fn with_artwork(mut self, artwork: Arc<dyn RuntimeArtwork>) -> Self {
        self.artwork = Some(artwork);
        self
    }

    #[must_use]
    pub fn with_lyrics(mut self, lyrics: Arc<dyn RuntimeLyrics>) -> Self {
        self.lyrics = Some(lyrics);
        self
    }

    #[must_use]
    pub fn with_notifier(mut self, notifier: Arc<dyn RuntimeNotifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    #[must_use]
    pub fn with_animation(mut self, animation: AnimationWorker) -> Self {
        self.animation = Some(animation);
        self
    }

    #[must_use]
    pub fn with_spectrum(mut self, spectrum: SpectrumWorker) -> Self {
        self.spectrum = Some(spectrum);
        self
    }

    #[must_use]
    pub fn with_terminal(mut self, terminal: Arc<dyn TerminalControl>) -> Self {
        self.terminal = Some(terminal);
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn RuntimeClock>) -> Self {
        self.clock = clock;
        self
    }

    #[must_use]
    pub fn with_initial_action(mut self, action: Action) -> Self {
        self.initial_actions.push(action);
        self
    }

    #[must_use]
    pub const fn with_action_capacity(mut self, capacity: usize) -> Self {
        self.action_capacity = capacity;
        self
    }

    #[must_use]
    pub const fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }
}

pub struct Runtime {
    config: Config,
    services: RuntimeServices,
}

fn reduce_actions_and_collect_effects(state: &mut AppState, actions: Vec<Action>) -> Vec<Effect> {
    let mut collected = Vec::new();
    for action in actions {
        let current = std::mem::take(state);
        let (next, effects) = reduce(current, action);
        *state = next;
        collected.extend(effects);
    }
    collected
}

#[async_trait]
#[allow(
    clippy::missing_errors_doc,
    reason = "each startup stage returns its boundary error unchanged to launch_application"
)]
pub trait StartupFactory: Send + Sync {
    fn resolve_paths(&self) -> anyhow::Result<AppPaths>;

    fn initialize_logging(&self, paths: &AppPaths) -> anyhow::Result<Box<dyn Send>>;

    fn load_config(&self, paths: &AppPaths) -> anyhow::Result<Config>;

    async fn migrate_storage(&self, paths: &AppPaths) -> anyhow::Result<Arc<dyn RuntimeStorage>>;

    async fn load_credentials(&self) -> anyhow::Result<Option<SecretString>>;

    async fn construct_provider(
        &self,
        credentials: Option<SecretString>,
    ) -> anyhow::Result<Arc<dyn MusicProvider>>;

    async fn check_dependencies(&self) -> anyhow::Result<DoctorReport>;

    async fn enter_tui(
        &self,
        paths: AppPaths,
        config: Config,
        storage: Arc<dyn RuntimeStorage>,
        provider: Arc<dyn MusicProvider>,
        dependencies: DoctorReport,
    ) -> anyhow::Result<()>;
}

/// Runs production startup stages before any alternate-screen mutation.
///
/// # Errors
///
/// Returns the first path, logging, configuration, migration, credential,
/// provider, dependency, or TUI error.
pub async fn launch_application(factory: &dyn StartupFactory) -> anyhow::Result<()> {
    let paths = factory.resolve_paths()?;
    let _logging_guard = factory.initialize_logging(&paths)?;
    let config = factory.load_config(&paths)?;
    let storage = factory.migrate_storage(&paths).await?;
    let credentials = factory.load_credentials().await?;
    let provider = factory.construct_provider(credentials).await?;
    let dependencies = factory.check_dependencies().await?;
    factory
        .enter_tui(paths, config, storage, provider, dependencies)
        .await
}

impl Runtime {
    #[must_use]
    pub const fn new(config: Config, services: RuntimeServices) -> Self {
        Self { config, services }
    }

    /// Restores durable state, renders it, and consumes runtime events.
    ///
    /// # Errors
    ///
    /// Returns a typed error when startup storage, state restoration, or
    /// rendering fails.
    ///
    /// # Panics
    ///
    /// [`RuntimeEvent::Panic`] deliberately resumes its injected panic after
    /// cleanup. It exists for lifecycle verification and embedding tests.
    #[allow(
        clippy::too_many_lines,
        reason = "the lifecycle remains one auditable acquire-run-cleanup-unwind sequence"
    )]
    pub async fn run<E, R>(self, event_source: E, mut renderer: R) -> Result<(), RuntimeError>
    where
        E: EventSource + 'static,
        R: Renderer,
    {
        configure_renderer_visualizer(&mut renderer, self.config.visualizer.max_fps);
        let mut state = if self.config.behavior.resume_session {
            match self.services.storage.load_session().await? {
                Some(checkpoint) => AppState::restore_session(self.config, checkpoint)?,
                None => AppState::new(self.config),
            }
        } else {
            AppState::new(self.config)
        };
        let mut initial_effects = Vec::new();
        for action in self.services.initial_actions {
            let (next, effects) = reduce(state, action);
            state = next;
            initial_effects.extend(effects);
        }
        let mut controller = UiController::default();
        controller.reconcile_state(&state);
        controller.reconcile_layout(&state, renderer.area());
        let mut animation = self.services.animation;
        let mut animation_redraw = animation.as_ref().map(AnimationWorker::redraw_receiver);
        reconcile_animation(&mut animation, &state, renderer.area());
        let mut spectrum = self.services.spectrum;
        let mut spectrum_redraw = spectrum.as_ref().map(SpectrumWorker::redraw_receiver);
        reconcile_spectrum(&mut spectrum, &state, renderer.area());
        let mut terminal = match self.services.terminal {
            Some(terminal) => Some(TerminalGuard::acquire(terminal)?),
            None => None,
        };
        let panic_hook = match terminal.as_ref() {
            Some(terminal) => Some(RuntimePanicHook::install(terminal.restorer()).await),
            None => None,
        };

        let shutdown = CancellationToken::new();
        let dispatch_cancel = CancellationToken::new();
        let terminal_control = TerminalControlPlane::new(dispatch_cancel.clone());
        let (actions, mut action_rx) = bounded_action_channel(self.services.action_capacity.max(1));
        let event_actions = actions.clone();
        let event_shutdown = shutdown.clone();
        let event_terminal = terminal_control.clone();
        let event_task = tokio::spawn(async move {
            pump_runtime_events(event_source, event_actions, event_shutdown, event_terminal).await;
        });
        let player_action_task = self.services.player_actions.map(|mut player_actions| {
            let player_sender = actions.clone();
            let player_shutdown = shutdown.clone();
            tokio::spawn(async move {
                while let Some(action) = player_actions.next_action().await {
                    if player_sender
                        .send_cancellable(RuntimeMessage::Action(action), &player_shutdown)
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            })
        });
        let mut dispatcher = EffectDispatcher::new(
            self.services.provider,
            self.services.podcast_rankings,
            self.services.player,
            self.services.account,
            self.services.dependencies,
            self.services.artwork,
            self.services.lyrics,
            self.services.notifier,
            self.services.storage,
            self.services.clock,
            actions,
            shutdown.clone(),
            dispatch_cancel.clone(),
        );
        dispatcher.dispatch(initial_effects).await;
        let motion_epoch = Instant::now();
        let mut progress_motion = ProgressMotion::default();
        let mut ui_motion = UiMotionTicker::spawn();
        let mut ui_motion_redraw = Some(ui_motion.redraw_receiver());
        let mut progress_authority = ProgressAuthority::from_state(&state);
        let mut pending_seek = false;

        let run_result = AssertUnwindSafe(async {
            render_ui_motion_frame(
                &mut renderer,
                &state,
                &mut controller,
                &mut progress_motion,
                motion_epoch,
                Some(ProgressChange::Media),
                &ui_motion,
            )?;
            loop {
                let Some(message) = receive_runtime_message(
                    &terminal_control.first,
                    &mut action_rx,
                    &mut animation_redraw,
                    &mut spectrum_redraw,
                    &mut ui_motion_redraw,
                )
                .await
                else {
                    break;
                };
                match message {
                    RuntimeMessage::Action(action) => {
                        let previous_position = state.playback().position_ms;
                        let previous_generation = state.current_attempt_generation();
                        let previous_media = state.playback().current.clone();
                        let current = std::mem::take(&mut state);
                        let (next, effects) = reduce(current, action);
                        state = next;
                        controller.reconcile_state(&state);
                        controller.reconcile_layout(&state, renderer.area());
                        reconcile_animation(&mut animation, &state, renderer.area());
                        reconcile_spectrum(&mut spectrum, &state, renderer.area());
                        reconcile_spectrum_discontinuity(
                            &mut spectrum,
                            previous_generation,
                            previous_media.as_ref(),
                            previous_position,
                            &state,
                        );
                        let next_authority = ProgressAuthority::from_state(&state);
                        let progress_change = progress_change_for_authority(
                            &progress_authority,
                            &next_authority,
                            pending_seek,
                        );
                        update_pending_seek(&mut pending_seek, &effects, progress_change);
                        progress_authority = next_authority;
                        render_ui_motion_frame(
                            &mut renderer,
                            &state,
                            &mut controller,
                            &mut progress_motion,
                            motion_epoch,
                            progress_change,
                            &ui_motion,
                        )?;
                        dispatcher.dispatch(effects).await;
                    }
                    RuntimeMessage::Key(key) => {
                        let acknowledged_key = key;
                        controller.reconcile_layout(&state, renderer.area());
                        reconcile_animation(&mut animation, &state, renderer.area());
                        reconcile_spectrum(&mut spectrum, &state, renderer.area());
                        let current = std::mem::take(&mut controller);
                        let (next, actions) = reduce_key(current, &state, key);
                        controller = next;
                        terminal_control.acknowledge_key(acknowledged_key, controller.input_mode());
                        let effects = reduce_actions_and_collect_effects(&mut state, actions);
                        controller.reconcile_state(&state);
                        controller.reconcile_layout(&state, renderer.area());
                        reconcile_animation(&mut animation, &state, renderer.area());
                        reconcile_spectrum(&mut spectrum, &state, renderer.area());
                        let next_authority = ProgressAuthority::from_state(&state);
                        let progress_change = progress_change_for_authority(
                            &progress_authority,
                            &next_authority,
                            pending_seek,
                        );
                        update_pending_seek(&mut pending_seek, &effects, progress_change);
                        progress_authority = next_authority;
                        render_ui_motion_frame(
                            &mut renderer,
                            &state,
                            &mut controller,
                            &mut progress_motion,
                            motion_epoch,
                            progress_change,
                            &ui_motion,
                        )?;
                        if controller.quit_requested() {
                            terminal_control.request(false);
                            break;
                        }
                        dispatcher.dispatch(effects).await;
                    }
                    RuntimeMessage::Mouse(mouse) => {
                        if !supported_mouse_kind(mouse.kind) {
                            continue;
                        }
                        controller.reconcile_layout(&state, renderer.area());
                        reconcile_animation(&mut animation, &state, renderer.area());
                        reconcile_spectrum(&mut spectrum, &state, renderer.area());
                        let snapshot = if matches!(
                            mouse.kind,
                            MouseEventKind::Down(crossterm::event::MouseButton::Left)
                        ) {
                            renderer.interaction_snapshot()
                        } else {
                            None
                        };
                        let current = std::mem::take(&mut controller);
                        let (next, actions) =
                            reduce_mouse(current, &state, mouse, snapshot.as_ref());
                        controller = next;
                        let effects = reduce_actions_and_collect_effects(&mut state, actions);
                        controller.reconcile_state(&state);
                        controller.reconcile_layout(&state, renderer.area());
                        reconcile_animation(&mut animation, &state, renderer.area());
                        reconcile_spectrum(&mut spectrum, &state, renderer.area());
                        let next_authority = ProgressAuthority::from_state(&state);
                        let progress_change = progress_change_for_authority(
                            &progress_authority,
                            &next_authority,
                            pending_seek,
                        );
                        update_pending_seek(&mut pending_seek, &effects, progress_change);
                        progress_authority = next_authority;
                        render_ui_motion_frame(
                            &mut renderer,
                            &state,
                            &mut controller,
                            &mut progress_motion,
                            motion_epoch,
                            progress_change,
                            &ui_motion,
                        )?;
                        if controller.quit_requested() {
                            terminal_control.request(false);
                            break;
                        }
                        dispatcher.dispatch(effects).await;
                    }
                    RuntimeMessage::Resize(_, _) => {
                        renderer.invalidate_interactions();
                        controller.reconcile_layout(&state, renderer.area());
                        reconcile_animation(&mut animation, &state, renderer.area());
                        reconcile_spectrum(&mut spectrum, &state, renderer.area());
                        render_ui_motion_frame(
                            &mut renderer,
                            &state,
                            &mut controller,
                            &mut progress_motion,
                            motion_epoch,
                            None,
                            &ui_motion,
                        )?;
                    }
                    RuntimeMessage::Redraw => {
                        controller.reconcile_layout(&state, renderer.area());
                        reconcile_animation(&mut animation, &state, renderer.area());
                        reconcile_spectrum(&mut spectrum, &state, renderer.area());
                        render_ui_motion_frame(
                            &mut renderer,
                            &state,
                            &mut controller,
                            &mut progress_motion,
                            motion_epoch,
                            None,
                            &ui_motion,
                        )?;
                    }
                    RuntimeMessage::Quit => {
                        terminal_control.request(false);
                        break;
                    }
                    RuntimeMessage::Panic => {
                        terminal_control.request(true);
                        panic!("injected runtime panic");
                    }
                }
            }
            Ok::<(), RuntimeError>(())
        })
        .catch_unwind()
        .await;

        let cleanup_deadline = Instant::now() + self.services.shutdown_timeout;
        shutdown.cancel();
        ui_motion.shutdown().await;
        if let Some(animation) = &mut animation {
            animation.shutdown().await;
        }
        if let Some(spectrum) = &mut spectrum {
            spectrum.shutdown().await;
        }
        terminal_control.wait_for_handoff(cleanup_deadline).await;
        if let Some(player_action_task) = player_action_task {
            player_action_task.abort();
            let _ = player_action_task.await;
        }
        action_rx.inner.close();
        dispatcher
            .flush_deferred_during_cleanup(cleanup_deadline)
            .await;
        let mut drained_panic = false;
        while let Some(message) = action_rx.recv().await {
            match message {
                RuntimeMessage::Action(action) => {
                    let current = std::mem::take(&mut state);
                    let (next, effects) = reduce(current, action);
                    state = next;
                    dispatcher
                        .dispatch_during_cleanup(effects, cleanup_deadline)
                        .await;
                }
                RuntimeMessage::Panic => drained_panic = true,
                RuntimeMessage::Key(_)
                | RuntimeMessage::Mouse(_)
                | RuntimeMessage::Resize(_, _)
                | RuntimeMessage::Redraw
                | RuntimeMessage::Quit => {}
            }
        }
        for message in terminal_control.take_interrupted() {
            match message {
                RuntimeMessage::Action(action) => {
                    let current = std::mem::take(&mut state);
                    let (next, effects) = reduce(current, action);
                    state = next;
                    dispatcher
                        .dispatch_during_cleanup(effects, cleanup_deadline)
                        .await;
                }
                RuntimeMessage::Panic => drained_panic = true,
                RuntimeMessage::Key(_)
                | RuntimeMessage::Mouse(_)
                | RuntimeMessage::Resize(_, _)
                | RuntimeMessage::Redraw
                | RuntimeMessage::Quit => {}
            }
        }
        if let Some(checkpoint) = coherent_session_checkpoint(&state) {
            dispatcher.schedule_final_checkpoint(checkpoint);
        }
        let account_shutdown = dispatcher.shutdown(cleanup_deadline).await;
        let terminal_result = match terminal.as_mut() {
            Some(terminal) => terminal.restore().map_err(RuntimeError::Render),
            None => Ok(()),
        };
        drop(panic_hook);
        let account_deadline = Instant::now() + self.services.shutdown_timeout;
        account_shutdown
            .finish(account_deadline, &terminal_control.emergency)
            .await;
        event_task.abort();
        let _ = event_task.await;

        match run_result {
            Ok(result) => {
                result?;
                if drained_panic || terminal_control.panic_requested() {
                    let _ = terminal_result;
                    panic!("injected runtime panic");
                }
                terminal_result
            }
            Err(payload) => {
                let _ = terminal_result;
                resume_unwind(payload);
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProgressAuthority {
    generation: Option<Generation>,
    media_id: Option<MediaId>,
    position_ms: u64,
    duration_ms: Option<u64>,
    status: PlaybackStatus,
}

impl ProgressAuthority {
    fn from_state(state: &AppState) -> Self {
        Self {
            generation: state.current_attempt_generation(),
            media_id: state.playback().current.clone(),
            position_ms: state.playback().position_ms,
            duration_ms: state.playback().duration_ms,
            status: state.playback().status,
        }
    }
}

fn progress_change_for_authority(
    previous: &ProgressAuthority,
    next: &ProgressAuthority,
    pending_seek: bool,
) -> Option<ProgressChange> {
    if previous == next {
        return None;
    }
    if previous.generation != next.generation || previous.media_id != next.media_id {
        Some(ProgressChange::Media)
    } else if pending_seek && previous.position_ms != next.position_ms {
        Some(ProgressChange::Seek)
    } else {
        Some(ProgressChange::Continuous)
    }
}

fn update_pending_seek(
    pending_seek: &mut bool,
    effects: &[Effect],
    progress_change: Option<ProgressChange>,
) {
    if matches!(
        progress_change,
        Some(ProgressChange::Seek | ProgressChange::Media)
    ) {
        *pending_seek = false;
    }
    if effects
        .iter()
        .any(|effect| matches!(effect, Effect::Player(PlayerCommand::SeekRelative { .. })))
    {
        *pending_seek = true;
    }
}

fn render_ui_motion_frame<R: Renderer>(
    renderer: &mut R,
    state: &AppState,
    controller: &mut UiController,
    progress: &mut ProgressMotion,
    epoch: Instant,
    progress_change: Option<ProgressChange>,
    ticker: &UiMotionTicker,
) -> io::Result<()> {
    let elapsed_ms = u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
    if let Some(change) = progress_change {
        progress.reconcile(
            elapsed_ms,
            state
                .current_attempt_generation()
                .map_or(0, Generation::value),
            state.playback().position_ms,
            state.playback().duration_ms,
            state.playback().status == PlaybackStatus::Playing,
            change,
        );
    }
    controller.set_motion_frame(MotionFrame {
        elapsed_ms,
        spinner_index: spinner_index(elapsed_ms),
        progress: progress.presentation(elapsed_ms),
    });
    renderer.render_with_model(state, controller.model())?;
    let demand = renderer.frame_motion_demand().unwrap_or_else(|| {
        state_motion_demand(state, controller.model()).merge(renderer.motion_demand())
    });
    ticker.set_demand(demand);
    Ok(())
}

fn state_motion_demand(state: &AppState, model: &RenderModel) -> MotionDemand {
    let progress = state.playback().status == PlaybackStatus::Playing
        && state
            .playback()
            .duration_ms
            .is_some_and(|duration| duration > 0 && state.playback().position_ms < duration);
    let spinner = if let Some(overlay) = model.overlay {
        overlay == crate::ui::render::Overlay::Lyrics && state.lyrics().loading()
    } else {
        match model.view {
            crate::ui::render::NavigationItem::Home
            | crate::ui::render::NavigationItem::Settings => false,
            crate::ui::render::NavigationItem::Search => {
                state.search().loading() || state.search().loading_more()
            }
            crate::ui::render::NavigationItem::Charts => state.charts().loading(),
            crate::ui::render::NavigationItem::Podcasts => {
                state.podcasts().recommendations_loading()
                    || state.podcasts().resolve_loading()
                    || state.podcasts().loading()
            }
            crate::ui::render::NavigationItem::Library => {
                state.library().loading() || state.library().loading_more()
            }
            crate::ui::render::NavigationItem::Favorites => state.favorites().loading(),
            crate::ui::render::NavigationItem::History => state.history().loading(),
        }
    };
    MotionDemand {
        progress,
        spinner,
        selection: false,
    }
}

async fn receive_runtime_message(
    terminal: &CancellationToken,
    action_rx: &mut ActionReceiver,
    animation_redraw: &mut Option<tokio::sync::watch::Receiver<u64>>,
    spectrum_redraw: &mut Option<tokio::sync::watch::Receiver<u64>>,
    ui_motion_redraw: &mut Option<tokio::sync::watch::Receiver<u64>>,
) -> Option<RuntimeMessage> {
    loop {
        let message = tokio::select! {
            biased;
            () = terminal.cancelled() => return None,
            message = action_rx.recv() => return message,
            redraw_open = redraw_receiver_changed(animation_redraw) => {
                redraw_open.then_some(RuntimeMessage::Redraw)
            }
            redraw_open = redraw_receiver_changed(spectrum_redraw) => {
                redraw_open.then_some(RuntimeMessage::Redraw)
            }
            redraw_open = redraw_receiver_changed(ui_motion_redraw) => {
                redraw_open.then_some(RuntimeMessage::Redraw)
            }
        };
        if message.is_some() {
            return message;
        }
    }
}

async fn redraw_receiver_changed(redraw: &mut Option<tokio::sync::watch::Receiver<u64>>) -> bool {
    let open = match redraw {
        Some(redraw) => redraw.changed().await.is_ok(),
        None => future::pending().await,
    };
    if !open {
        *redraw = None;
    }
    open
}

fn animation_key_for_state(state: &AppState, size: CellSize) -> Option<AnimationKey> {
    let generation = state.current_attempt_generation()?;
    let media_id = state.playback().current.as_ref()?;
    let current = state.queue().current()?.media();
    (current.kind == MediaKind::Video && &current.id == media_id)
        .then(|| AnimationKey::new(generation, media_id.clone(), size))
}

fn animation_request_for_state(state: &AppState, area: Option<Rect>) -> Option<AnimationRequest> {
    if !state.animated_artwork_enabled()
        || area.is_none_or(|area| LayoutMode::for_area(area) != LayoutMode::Wide)
    {
        return None;
    }
    let key = animation_key_for_state(state, PRODUCTION_ARTWORK_SIZE)?;
    let preview = state.player_presentation().preview_url()?.clone();
    Some(AnimationRequest::new(key, preview).with_start_ms(state.playback().position_ms))
}

fn reconcile_animation(
    animation: &mut Option<AnimationWorker>,
    state: &AppState,
    area: Option<Rect>,
) {
    let Some(worker) = animation else {
        return;
    };
    match state.playback().status {
        PlaybackStatus::Playing => match animation_request_for_state(state, area) {
            Some(request) if worker.active_key() == Some(request.key()) => {
                worker.resume(state.playback().position_ms);
            }
            Some(request) => worker.replace(request),
            None => worker.clear(),
        },
        PlaybackStatus::Paused => {
            let key = animation_key_for_state(state, PRODUCTION_ARTWORK_SIZE);
            if area.is_some_and(|area| LayoutMode::for_area(area) == LayoutMode::Wide)
                && key.as_ref() == worker.active_key()
            {
                worker.pause();
            } else {
                worker.clear();
            }
        }
        PlaybackStatus::Stopped
        | PlaybackStatus::Resolving
        | PlaybackStatus::Buffering
        | PlaybackStatus::Failed => worker.clear(),
    }
}

fn spectrum_key_for_state(state: &AppState, target: SpectrumTarget) -> Option<SpectrumKey> {
    let generation = state.current_attempt_generation()?;
    let media_id = state.playback().current.as_ref()?;
    let current = state.queue().current()?.media();
    (&current.id == media_id).then(|| SpectrumKey::new(generation, media_id.clone(), target))
}

fn spectrum_target_for_area(area: Option<Rect>) -> Option<SpectrumTarget> {
    let area = area?;
    let rows = match LayoutMode::for_area(area) {
        LayoutMode::Wide => 3,
        LayoutMode::Compact => 1,
        LayoutMode::Tiny => return None,
    };
    let maximum_bands = u16::try_from(MAX_SPECTRUM_BANDS).ok()?;
    let bands = area.width.clamp(1, maximum_bands);
    SpectrumTarget::new(bands, rows)
}

fn spectrum_request_for_state(state: &AppState, area: Option<Rect>) -> Option<SpectrumRequest> {
    if !state.visualizer_enabled() {
        return None;
    }
    let target = spectrum_target_for_area(area)?;
    let key = spectrum_key_for_state(state, target)?;
    let stream_url = state.player_presentation().analysis_url()?.clone();
    Some(SpectrumRequest::new(key, stream_url).with_start_ms(state.playback().position_ms))
}

fn reconcile_spectrum(spectrum: &mut Option<SpectrumWorker>, state: &AppState, area: Option<Rect>) {
    let Some(worker) = spectrum else {
        return;
    };
    match state.playback().status {
        PlaybackStatus::Playing => match spectrum_request_for_state(state, area) {
            Some(request) if worker.active_request_matches(&request) => {
                worker.resume(state.playback().position_ms);
            }
            Some(request) => worker.replace(request),
            None => worker.clear(),
        },
        PlaybackStatus::Paused => {
            let key = spectrum_target_for_area(area)
                .and_then(|target| spectrum_key_for_state(state, target));
            if state.visualizer_enabled() && key.as_ref() == worker.active_key() {
                worker.pause();
            } else {
                worker.clear();
            }
        }
        PlaybackStatus::Stopped
        | PlaybackStatus::Resolving
        | PlaybackStatus::Buffering
        | PlaybackStatus::Failed => worker.clear(),
    }
}

const SPECTRUM_SEEK_DISCONTINUITY_MS: u64 = 2_000;

fn reconcile_spectrum_discontinuity(
    spectrum: &mut Option<SpectrumWorker>,
    previous_generation: Option<Generation>,
    previous_media: Option<&MediaId>,
    previous_position_ms: u64,
    state: &AppState,
) {
    if state.playback().status != PlaybackStatus::Playing
        || previous_generation != state.current_attempt_generation()
        || previous_media != state.playback().current.as_ref()
        || previous_position_ms.abs_diff(state.playback().position_ms)
            <= SPECTRUM_SEEK_DISCONTINUITY_MS
    {
        return;
    }
    if let Some(worker) = spectrum {
        worker.seek(state.playback().position_ms);
    }
}

fn coherent_session_checkpoint(state: &AppState) -> Option<SessionCheckpoint> {
    let queue_current = state.queue().current().map(|item| &item.media().id);
    (state.podcasts().pending_progress_generation().is_none()
        && queue_current == state.playback().current.as_ref())
    .then(|| SessionCheckpoint {
        queue: state.queue().snapshot(),
        playback: state.playback().clone(),
    })
}

struct ReplaceableTask {
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl ReplaceableTask {
    async fn cancel(self) {
        self.cancel.cancel();
        self.handle.abort();
        let _ = self.handle.await;
    }
}

async fn replace_task<F>(slot: &mut Option<ReplaceableTask>, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Some(task) = slot.take() {
        task.cancel().await;
    }
    *slot = Some(ReplaceableTask {
        cancel: CancellationToken::new(),
        handle: tokio::spawn(future),
    });
}

struct EffectDispatcher {
    provider: Option<Arc<dyn MusicProvider>>,
    podcast_rankings: Option<Arc<dyn PodcastRankingSource>>,
    dependencies: Option<Arc<dyn RuntimeDependencies>>,
    artwork: Option<Arc<dyn RuntimeArtwork>>,
    lyrics: Option<Arc<dyn RuntimeLyrics>>,
    notifications: Option<NotificationWorker>,
    storage: Arc<dyn RuntimeStorage>,
    clock: Arc<dyn RuntimeClock>,
    actions: ActionSender,
    shutdown: CancellationToken,
    dispatch_cancel: CancellationToken,
    search: Option<ReplaceableTask>,
    artwork_task: Option<ReplaceableTask>,
    lyrics_task: Option<ReplaceableTask>,
    chart_task: Option<ReplaceableTask>,
    chart_cache_task: Option<ReplaceableTask>,
    podcast_task: Option<ReplaceableTask>,
    podcast_recommendation_task: Option<ReplaceableTask>,
    podcast_recommendation_match_task: Option<ReplaceableTask>,
    library_task: Option<ReplaceableTask>,
    dependencies_task: Option<ReplaceableTask>,
    radio_task: Option<ReplaceableTask>,
    diagnostic_task: Option<ReplaceableTask>,
    sessions: SessionPersister,
    ordered_storage: OrderedStorageEffects,
    ordered_player: OrderedPlayerEffects,
    ordered_account: OrderedAccountEffects,
    deferred_storage: VecDeque<OrderedStorageEffect>,
}

impl EffectDispatcher {
    #[allow(
        clippy::too_many_arguments,
        reason = "construction makes every independently injected runtime boundary explicit"
    )]
    fn new(
        provider: Option<Arc<dyn MusicProvider>>,
        podcast_rankings: Option<Arc<dyn PodcastRankingSource>>,
        player: Option<Arc<dyn RuntimePlayer>>,
        account: Option<Arc<dyn RuntimeAccount>>,
        dependencies: Option<Arc<dyn RuntimeDependencies>>,
        artwork: Option<Arc<dyn RuntimeArtwork>>,
        lyrics: Option<Arc<dyn RuntimeLyrics>>,
        notifier: Option<Arc<dyn RuntimeNotifier>>,
        storage: Arc<dyn RuntimeStorage>,
        clock: Arc<dyn RuntimeClock>,
        actions: ActionSender,
        shutdown: CancellationToken,
        dispatch_cancel: CancellationToken,
    ) -> Self {
        let sessions = SessionPersister::spawn(Arc::clone(&storage), Arc::clone(&clock));
        let ordered_storage = OrderedStorageEffects::spawn(
            Arc::clone(&storage),
            Arc::clone(&clock),
            actions.clone(),
            shutdown.clone(),
        );
        let ordered_player = OrderedPlayerEffects::spawn(player, actions.clone(), shutdown.clone());
        let ordered_account =
            OrderedAccountEffects::spawn(account, actions.clone(), shutdown.clone());
        Self {
            provider,
            podcast_rankings,
            dependencies,
            artwork,
            lyrics,
            notifications: notifier
                .map(|notifier| NotificationWorker::new(notifier, NOTIFICATION_TIMEOUT)),
            storage,
            clock,
            actions,
            shutdown,
            dispatch_cancel,
            search: None,
            artwork_task: None,
            lyrics_task: None,
            chart_task: None,
            chart_cache_task: None,
            podcast_task: None,
            podcast_recommendation_task: None,
            podcast_recommendation_match_task: None,
            library_task: None,
            dependencies_task: None,
            radio_task: None,
            diagnostic_task: None,
            sessions,
            ordered_storage,
            ordered_player,
            ordered_account,
            deferred_storage: VecDeque::new(),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive effect dispatcher stays in one auditable match"
    )]
    async fn dispatch(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Search {
                    generation,
                    query,
                    filter,
                } => {
                    self.replace_search(generation, query, filter, None).await;
                }
                Effect::SearchMore {
                    generation,
                    query,
                    filter,
                    continuation,
                } => {
                    self.replace_search(generation, query, filter, Some(continuation))
                        .await;
                }
                Effect::LoadCharts { generation, region } => {
                    let provider = self.provider.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    let clock = Arc::clone(&self.clock);
                    replace_task(&mut self.chart_task, async move {
                        let result = match provider {
                            Some(provider) => provider.charts(&region).await.map_err(|error| {
                                provider_app_error(AppErrorCategory::Charts, error)
                            }),
                            None => Err(unavailable_boundary(AppErrorCategory::Charts)),
                        };
                        let action = Action::ChartsCompleted {
                            generation,
                            region,
                            received_at: clock.now_millis(),
                            result,
                        };
                        let _ = actions
                            .send_cancellable(RuntimeMessage::Action(action), &shutdown)
                            .await;
                    })
                    .await;
                }
                Effect::ReadChartCache {
                    generation,
                    region,
                    key,
                } => {
                    let storage = Arc::clone(&self.storage);
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    let clock = Arc::clone(&self.clock);
                    replace_task(&mut self.chart_cache_task, async move {
                        let result = match storage.load_chart_cache(&key.to_string()).await {
                            Ok(Some(entry)) => {
                                ChartCachePayload::from_metadata_entry(&region, &entry).map(Some)
                            }
                            Ok(None) => Ok(None),
                            Err(_) => Err(AppError::new(
                                AppErrorCategory::Charts,
                                "chart cache could not be read",
                            )),
                        };
                        let action = Action::CachedChartsCompleted {
                            generation,
                            region,
                            observed_at: clock.now_millis(),
                            result,
                        };
                        let _ = actions
                            .send_cancellable(RuntimeMessage::Action(action), &shutdown)
                            .await;
                    })
                    .await;
                }
                Effect::StoreChartCache { key, payload } => match payload.encoded() {
                    Ok(encoded) => {
                        let command = OrderedStorageEffect::StoreChartCache {
                            key: key.to_string(),
                            encoded,
                            expires_at: payload.expires_at(),
                            stored_at: payload.stored_at(),
                        };
                        if let Err(command) = self
                            .ordered_storage
                            .schedule(command, &self.dispatch_cancel)
                            .await
                        {
                            self.deferred_storage.push_back(command);
                        }
                    }
                    Err(error) => {
                        self.schedule_diagnostic(
                            DiagnosticCategory::State,
                            error.message().to_owned(),
                        )
                        .await;
                    }
                },
                Effect::LoadPodcast { generation, id } => {
                    let provider = self.provider.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    replace_task(&mut self.podcast_task, async move {
                        let result = match provider {
                            Some(provider) => {
                                provider.podcast(id.as_str()).await.map_err(|error| {
                                    provider_app_error(AppErrorCategory::Podcast, error)
                                })
                            }
                            None => Err(unavailable_boundary(AppErrorCategory::Podcast)),
                        };
                        let _ = actions
                            .send_cancellable(
                                RuntimeMessage::Action(Action::PodcastCompleted {
                                    generation,
                                    result,
                                }),
                                &shutdown,
                            )
                            .await;
                    })
                    .await;
                }
                Effect::LoadPodcastRecommendations { generation, region } => {
                    let source = self.podcast_rankings.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    replace_task(&mut self.podcast_recommendation_task, async move {
                        let result = match source {
                            Some(source) => source
                                .top_shows(&region)
                                .await
                                .map_err(podcast_ranking_app_error),
                            None => Err(unavailable_boundary(AppErrorCategory::Podcast)),
                        };
                        let _ = actions
                            .send_cancellable(
                                RuntimeMessage::Action(Action::PodcastRecommendationsCompleted {
                                    generation,
                                    requested_region: region,
                                    result,
                                }),
                                &shutdown,
                            )
                            .await;
                    })
                    .await;
                }
                Effect::ResolvePodcastRecommendation {
                    generation,
                    recommendation,
                } => {
                    let provider = self.provider.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    replace_task(&mut self.podcast_recommendation_match_task, async move {
                        let result = match provider {
                            Some(provider) => {
                                let query = podcast_match_query(&recommendation);
                                if query.trim().is_empty() {
                                    Err(podcast_match_unavailable())
                                } else {
                                    match provider.search(&query, SearchFilter::Podcasts).await {
                                        Ok(page) => {
                                            let candidate_count =
                                                page.items.len().min(MAX_PODCAST_MATCH_CANDIDATES);
                                            match_podcast_recommendation(
                                                &recommendation,
                                                &page.items[..candidate_count],
                                            )
                                            .ok_or_else(podcast_match_unavailable)
                                        }
                                        Err(_) => Err(podcast_match_unavailable()),
                                    }
                                }
                            }
                            None => Err(unavailable_boundary(AppErrorCategory::Podcast)),
                        };
                        let _ = actions
                            .send_cancellable(
                                RuntimeMessage::Action(Action::PodcastRecommendationResolved {
                                    generation,
                                    result,
                                }),
                                &shutdown,
                            )
                            .await;
                    })
                    .await;
                }
                Effect::Persist(checkpoint) => self.sessions.schedule(checkpoint),
                Effect::SavePodcastProgress(checkpoint) => {
                    if let Err(command) = self
                        .ordered_storage
                        .schedule(
                            OrderedStorageEffect::SavePodcast(checkpoint),
                            &self.dispatch_cancel,
                        )
                        .await
                    {
                        self.deferred_storage.push_back(command);
                    }
                }
                Effect::LoadPodcastProgress {
                    generation,
                    media_id,
                } => {
                    let _ = self
                        .ordered_storage
                        .schedule(
                            OrderedStorageEffect::LoadPodcast {
                                generation,
                                video_id: media_id.video_id,
                            },
                            &self.dispatch_cancel,
                        )
                        .await;
                }
                Effect::ConnectAccount { browser } => {
                    self.ordered_account
                        .schedule(browser, &self.dispatch_cancel)
                        .await;
                }
                Effect::LoadLibrary {
                    generation,
                    section,
                    continuation,
                } => {
                    let provider = self.provider.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    replace_task(&mut self.library_task, async move {
                        let result = match provider {
                            Some(provider) => {
                                let result = match continuation {
                                    Some(continuation) => {
                                        provider.library_more(section, continuation.as_str()).await
                                    }
                                    None => provider.library(section).await,
                                };
                                result.map_err(|error| {
                                    provider_app_error(AppErrorCategory::Library, error)
                                })
                            }
                            None => Err(unavailable_boundary(AppErrorCategory::Library)),
                        };
                        let _ = actions
                            .send_cancellable(
                                RuntimeMessage::Action(Action::LibraryCompleted {
                                    generation,
                                    result,
                                }),
                                &shutdown,
                            )
                            .await;
                    })
                    .await;
                }
                Effect::CheckDependencies => {
                    let dependencies = self.dependencies.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    replace_task(&mut self.dependencies_task, async move {
                        let report = match dependencies {
                            Some(dependencies) => dependencies.check().await,
                            None => unavailable_dependency_report(),
                        };
                        let _ = actions
                            .send_cancellable(
                                RuntimeMessage::Action(Action::DependencyReportLoaded(report)),
                                &shutdown,
                            )
                            .await;
                    })
                    .await;
                }
                Effect::LoadHistory { generation, limit } => {
                    let _ = self
                        .ordered_storage
                        .schedule(
                            OrderedStorageEffect::LoadHistory { generation, limit },
                            &self.dispatch_cancel,
                        )
                        .await;
                }
                Effect::LoadFavorites { generation } => {
                    let _ = self
                        .ordered_storage
                        .schedule(
                            OrderedStorageEffect::LoadFavorites { generation },
                            &self.dispatch_cancel,
                        )
                        .await;
                }
                Effect::AddFavorite { generation, item } => {
                    let _ = self
                        .ordered_storage
                        .schedule(
                            OrderedStorageEffect::AddFavorite { generation, item },
                            &self.dispatch_cancel,
                        )
                        .await;
                }
                Effect::RemoveFavorite {
                    generation,
                    media_id,
                } => {
                    let _ = self
                        .ordered_storage
                        .schedule(
                            OrderedStorageEffect::RemoveFavorite {
                                generation,
                                media_id,
                            },
                            &self.dispatch_cancel,
                        )
                        .await;
                }
                Effect::RecordHistory { item } => {
                    if let Err(command) = self
                        .ordered_storage
                        .schedule(
                            OrderedStorageEffect::RecordHistory(item),
                            &self.dispatch_cancel,
                        )
                        .await
                    {
                        self.deferred_storage.push_back(command);
                    }
                }
                Effect::Notify(notification) => {
                    if let Some(notifications) = &mut self.notifications {
                        notifications.replace(notification);
                    }
                }
                Effect::Resolve {
                    generation,
                    item,
                    start_ms,
                } => {
                    self.ordered_player
                        .schedule(
                            OrderedPlayerEffect::Play {
                                generation,
                                item: Box::new(item),
                                start_ms,
                            },
                            &self.dispatch_cancel,
                        )
                        .await;
                }
                Effect::Player(command) => {
                    self.ordered_player
                        .schedule(OrderedPlayerEffect::Command(command), &self.dispatch_cancel)
                        .await;
                }
                Effect::FillRadio { generation, seed } => {
                    let provider = self.provider.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    replace_task(&mut self.radio_task, async move {
                        let result = match provider {
                            Some(provider) => provider.radio(&seed).await.map_err(|error| {
                                provider_app_error(AppErrorCategory::Radio, error)
                            }),
                            None => Err(unavailable_boundary(AppErrorCategory::Radio)),
                        };
                        let _ = actions
                            .send_cancellable(
                                RuntimeMessage::Action(Action::RadioFillCompleted {
                                    generation,
                                    result,
                                }),
                                &shutdown,
                            )
                            .await;
                    })
                    .await;
                }
                Effect::FetchArtwork { generation, url } => {
                    if let Some(task) = self.artwork_task.take() {
                        task.cancel().await;
                    }
                    let artwork = self.artwork.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    if let Some(artwork) = &artwork {
                        artwork.request(generation, &url);
                    }
                    let cancel = CancellationToken::new();
                    let handle = tokio::spawn(async move {
                        let result = match artwork {
                            Some(artwork) => artwork.fetch(generation, url).await.map_err(|_| {
                                AppError::new(
                                    AppErrorCategory::Artwork,
                                    "artwork could not be loaded",
                                )
                            }),
                            None => Err(unavailable_boundary(AppErrorCategory::Artwork)),
                        };
                        let _ = actions
                            .send_cancellable(
                                RuntimeMessage::Action(Action::ArtworkCompleted {
                                    generation,
                                    result,
                                }),
                                &shutdown,
                            )
                            .await;
                    });
                    self.artwork_task = Some(ReplaceableTask { cancel, handle });
                }
                Effect::ClearArtwork => {
                    if let Some(task) = self.artwork_task.take() {
                        task.cancel().await;
                    }
                    if let Some(artwork) = &self.artwork {
                        artwork.clear();
                    }
                }
                Effect::LoadLyrics { generation, item } => {
                    let item = item.into_media();
                    let lyrics = self.lyrics.clone();
                    let actions = self.actions.clone();
                    let shutdown = self.shutdown.clone();
                    let media_id = item.id.clone();
                    replace_task(&mut self.lyrics_task, async move {
                        let result = match lyrics {
                            Some(lyrics) => lyrics.load(&item).await.map_err(|_| {
                                AppError::new(
                                    AppErrorCategory::Lyrics,
                                    "lyrics could not be loaded",
                                )
                            }),
                            None => Err(AppError::new(
                                AppErrorCategory::Lyrics,
                                "lyrics service is unavailable",
                            )),
                        };
                        let _ = actions
                            .send_cancellable(
                                RuntimeMessage::Action(Action::LyricsCompleted {
                                    generation,
                                    media_id: media_id.into(),
                                    result,
                                }),
                                &shutdown,
                            )
                            .await;
                    })
                    .await;
                }
                Effect::ClearLyrics => {
                    if let Some(task) = self.lyrics_task.take() {
                        task.cancel().await;
                    }
                }
            }
        }
    }

    async fn dispatch_during_cleanup(&mut self, effects: Vec<Effect>, deadline: Instant) {
        let cleanup = CancellationToken::new();
        for effect in effects {
            if Instant::now() >= deadline {
                return;
            }
            match effect {
                Effect::Persist(checkpoint) => self.sessions.schedule(checkpoint),
                Effect::StoreChartCache { key, payload } => {
                    if let Ok(encoded) = payload.encoded() {
                        let _ = tokio::time::timeout_at(
                            deadline,
                            self.ordered_storage.schedule(
                                OrderedStorageEffect::StoreChartCache {
                                    key: key.to_string(),
                                    encoded,
                                    expires_at: payload.expires_at(),
                                    stored_at: payload.stored_at(),
                                },
                                &cleanup,
                            ),
                        )
                        .await;
                    }
                }
                Effect::SavePodcastProgress(checkpoint) => {
                    let _ = tokio::time::timeout_at(
                        deadline,
                        self.ordered_storage
                            .schedule(OrderedStorageEffect::SavePodcast(checkpoint), &cleanup),
                    )
                    .await;
                }
                Effect::RecordHistory { item } => {
                    let _ = tokio::time::timeout_at(
                        deadline,
                        self.ordered_storage
                            .schedule(OrderedStorageEffect::RecordHistory(item), &cleanup),
                    )
                    .await;
                }
                _ => {}
            }
        }
    }

    async fn flush_deferred_during_cleanup(&mut self, deadline: Instant) {
        let cleanup = CancellationToken::new();
        while Instant::now() < deadline {
            let Some(command) = self.deferred_storage.pop_front() else {
                return;
            };
            if tokio::time::timeout_at(deadline, self.ordered_storage.schedule(command, &cleanup))
                .await
                .is_err()
            {
                return;
            }
        }
    }

    async fn schedule_diagnostic(&mut self, category: DiagnosticCategory, message: String) {
        let actions = self.actions.clone();
        let shutdown = self.shutdown.clone();
        replace_task(&mut self.diagnostic_task, async move {
            send_runtime_diagnostic(&actions, &shutdown, category, &message).await;
        })
        .await;
    }

    async fn replace_search(
        &mut self,
        generation: Generation,
        query: String,
        filter: crate::domain::SearchFilter,
        continuation: Option<OpaqueContinuation>,
    ) {
        if let Some(task) = self.search.take() {
            task.cancel().await;
        }
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let shutdown = self.shutdown.clone();
        let actions = self.actions.clone();
        let provider = self.provider.clone();
        let handle = tokio::spawn(async move {
            let result = match provider {
                Some(provider) => tokio::select! {
                    biased;
                    () = task_cancel.cancelled() => return,
                    result = async {
                        match continuation {
                            Some(continuation) => provider
                                .search_more(&query, filter, continuation.as_str())
                                .await,
                            None => provider.search(&query, filter).await,
                        }
                    } => result
                        .map(SearchPage::from_provider)
                        .map_err(|error| provider_app_error(AppErrorCategory::Search, error)),
                },
                None => Err(unavailable_boundary(AppErrorCategory::Search)),
            };
            let action = Action::SearchCompleted { generation, result };
            let _ = actions
                .send_cancellable(RuntimeMessage::Action(action), &shutdown)
                .await;
        });
        self.search = Some(ReplaceableTask { cancel, handle });
    }

    fn schedule_final_checkpoint(&self, checkpoint: SessionCheckpoint) {
        self.sessions.schedule(checkpoint);
    }

    async fn shutdown(mut self, deadline: Instant) -> PostTerminalAccountShutdown {
        if let Some(task) = self.search.take() {
            task.cancel().await;
        }
        if let Some(task) = self.artwork_task.take() {
            task.cancel().await;
        }
        if let Some(task) = self.lyrics_task.take() {
            task.cancel().await;
        }
        if let Some(notifications) = &mut self.notifications {
            notifications.shutdown(deadline).await;
        }
        for task in [
            self.chart_task.take(),
            self.chart_cache_task.take(),
            self.podcast_task.take(),
            self.podcast_recommendation_task.take(),
            self.podcast_recommendation_match_task.take(),
            self.library_task.take(),
            self.dependencies_task.take(),
            self.radio_task.take(),
            self.diagnostic_task.take(),
        ]
        .into_iter()
        .flatten()
        {
            task.cancel().await;
        }
        let account_shutdown = self.ordered_account.begin_shutdown(deadline).await;
        self.ordered_storage.shutdown(deadline).await;
        self.sessions.shutdown(deadline).await;
        self.ordered_player.shutdown(deadline).await;
        account_shutdown
    }
}

fn podcast_ranking_app_error(error: PodcastRankingError) -> AppError {
    let message = match error {
        PodcastRankingError::Unavailable => "podcast rankings are unavailable",
        PodcastRankingError::InvalidResponse => "podcast ranking response is invalid",
        PodcastRankingError::TooLarge => "podcast ranking response exceeds the input limit",
    };
    AppError::new(AppErrorCategory::Podcast, message)
}

fn podcast_match_unavailable() -> AppError {
    AppError::new(
        AppErrorCategory::Podcast,
        "podcast recommendation could not be matched",
    )
}

fn podcast_match_query(recommendation: &crate::podcast_rankings::PodcastRecommendation) -> String {
    let component_bytes = (MAX_PODCAST_MATCH_QUERY_BYTES - 1) / 2;
    let component_graphemes = (MAX_PODCAST_MATCH_QUERY_GRAPHEMES - 1) / 2;
    let title =
        bounded_query_component(recommendation.title(), component_bytes, component_graphemes);
    let publisher = bounded_query_component(
        recommendation.publisher(),
        component_bytes,
        component_graphemes,
    );
    match (title.is_empty(), publisher.is_empty()) {
        (false, false) => format!("{title} {publisher}"),
        (false, true) => title,
        (true, false) => publisher,
        (true, true) => String::new(),
    }
}

fn bounded_query_component(value: &str, max_bytes: usize, max_graphemes: usize) -> String {
    let mut bounded = String::new();
    for grapheme in value.trim().graphemes(true).take(max_graphemes) {
        if bounded.len().saturating_add(grapheme.len()) > max_bytes {
            break;
        }
        bounded.push_str(grapheme);
    }
    bounded
}

enum OrderedAccountEffect {
    Connect(Browser),
}

struct OrderedAccountEffects {
    commands: mpsc::Sender<OrderedAccountEffect>,
    task: JoinHandle<()>,
    account: Option<Arc<dyn RuntimeAccount>>,
}

impl OrderedAccountEffects {
    fn spawn(
        account: Option<Arc<dyn RuntimeAccount>>,
        actions: ActionSender,
        shutdown: CancellationToken,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(ORDERED_ACCOUNT_CAPACITY);
        let task = tokio::spawn(ordered_account_loop(
            receiver,
            account.clone(),
            actions,
            shutdown,
        ));
        Self {
            commands,
            task,
            account,
        }
    }

    async fn schedule(&self, browser: Browser, cancel: &CancellationToken) {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {}
            _ = self.commands.send(OrderedAccountEffect::Connect(browser)) => {}
        }
    }

    async fn begin_shutdown(self, deadline: Instant) -> PostTerminalAccountShutdown {
        let Self {
            commands: _,
            mut task,
            account,
        } = self;
        task.abort();
        let worker = if tokio::time::timeout_at(deadline, &mut task).await.is_ok() {
            None
        } else {
            Some(task)
        };
        PostTerminalAccountShutdown { worker, account }
    }
}

struct PostTerminalAccountShutdown {
    worker: Option<JoinHandle<()>>,
    account: Option<Arc<dyn RuntimeAccount>>,
}

impl PostTerminalAccountShutdown {
    async fn finish(mut self, deadline: Instant, emergency: &CancellationToken) {
        if let Some(mut worker) = self.worker.take() {
            let completed = tokio::select! {
                biased;
                () = emergency.cancelled() => false,
                result = tokio::time::timeout_at(deadline, &mut worker) => result.is_ok(),
            };
            if !completed {
                worker.abort();
                let _ = worker.await;
            }
        }
        if let Some(account) = self.account.take() {
            let mut shutdown = Box::pin(account.shutdown());
            tokio::select! {
                biased;
                () = emergency.cancelled() => {}
                _ = tokio::time::timeout_at(deadline, &mut shutdown) => {}
            }
            // Credential commit is the recoverable transaction boundary: the
            // production keyring write is atomic, and the next launch rebuilds
            // the provider from whatever credential was committed. Dropping
            // the final account owner aborts/reaps the async wrapper without
            // exposing a half-swapped provider to a still-running application.
            drop(shutdown);
            drop(account);
        }
    }
}

impl Drop for PostTerminalAccountShutdown {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

async fn ordered_account_loop(
    mut commands: mpsc::Receiver<OrderedAccountEffect>,
    account: Option<Arc<dyn RuntimeAccount>>,
    actions: ActionSender,
    shutdown: CancellationToken,
) {
    while let Some(effect) = commands.recv().await {
        match effect {
            OrderedAccountEffect::Connect(browser) => match &account {
                Some(account) => match account.connect(browser).await {
                    Ok(authentication) => {
                        let _ = actions
                            .send_internal_cancellable(
                                RuntimeMessage::Action(Action::AuthenticationChanged(
                                    authentication,
                                )),
                                &shutdown,
                            )
                            .await;
                    }
                    Err(_) => {
                        send_internal_runtime_diagnostic(
                            &actions,
                            &shutdown,
                            DiagnosticCategory::State,
                            "account connection failed",
                        )
                        .await;
                    }
                },
                None => {
                    send_internal_runtime_diagnostic(
                        &actions,
                        &shutdown,
                        DiagnosticCategory::State,
                        "account connection is unavailable",
                    )
                    .await;
                }
            },
        }
    }
}

enum OrderedPlayerEffect {
    Play {
        generation: Generation,
        item: Box<MediaItem>,
        start_ms: Option<u64>,
    },
    Command(PlayerCommand),
    Shutdown {
        started: oneshot::Sender<()>,
        completed: oneshot::Sender<()>,
    },
}

struct OrderedPlayerEffects {
    commands: mpsc::Sender<OrderedPlayerEffect>,
    task: JoinHandle<()>,
    player: Option<Arc<dyn RuntimePlayer>>,
}

impl OrderedPlayerEffects {
    fn spawn(
        player: Option<Arc<dyn RuntimePlayer>>,
        actions: ActionSender,
        shutdown: CancellationToken,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(ORDERED_PLAYER_CAPACITY);
        let task = tokio::spawn(ordered_player_loop(
            receiver,
            player.clone(),
            actions,
            shutdown,
        ));
        Self {
            commands,
            task,
            player,
        }
    }

    async fn schedule(&self, effect: OrderedPlayerEffect, shutdown: &CancellationToken) {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {}
            _ = self.commands.send(effect) => {}
        }
    }

    async fn shutdown(self, deadline: Instant) {
        let (started_tx, mut started_rx) = oneshot::channel();
        let (completed, result) = oneshot::channel();
        let _ = tokio::time::timeout_at(
            deadline,
            self.commands.send(OrderedPlayerEffect::Shutdown {
                started: started_tx,
                completed,
            }),
        )
        .await;
        if matches!(tokio::time::timeout_at(deadline, result).await, Ok(Ok(()))) {
            let _ = self.task.await;
        } else {
            let started = started_rx.try_recv().is_ok();
            self.task.abort();
            let _ = self.task.await;
            if let Some(player) = &self.player {
                if !started {
                    let mut shutdown = Box::pin(player.shutdown());
                    if futures::poll!(&mut shutdown).is_ready() {
                        return;
                    }
                }
                player.abort();
            }
        }
    }
}

async fn ordered_player_loop(
    mut commands: mpsc::Receiver<OrderedPlayerEffect>,
    player: Option<Arc<dyn RuntimePlayer>>,
    actions: ActionSender,
    shutdown: CancellationToken,
) {
    while let Some(effect) = commands.recv().await {
        match effect {
            OrderedPlayerEffect::Play {
                generation,
                item,
                start_ms,
            } => {
                let result = match &player {
                    Some(player) => player.play(generation, *item, start_ms).await,
                    None => Err(RuntimePlayerError),
                };
                if result.is_err() {
                    let _ = actions
                        .send_internal_cancellable(
                            RuntimeMessage::Action(Action::ResolveFailed {
                                generation,
                                error: AppError::new(
                                    AppErrorCategory::Resolve,
                                    "player could not start playback",
                                ),
                            }),
                            &shutdown,
                        )
                        .await;
                }
            }
            OrderedPlayerEffect::Command(command) => {
                let result = match (&player, command) {
                    (Some(player), PlayerCommand::Pause) => player.pause().await,
                    (Some(player), PlayerCommand::Resume) => player.resume().await,
                    (Some(player), PlayerCommand::Volume(volume)) => {
                        player.set_volume(volume).await
                    }
                    (Some(player), PlayerCommand::SeekRelative { seconds }) => {
                        player.seek_relative(seconds).await
                    }
                    (None, _) => Err(RuntimePlayerError),
                };
                if result.is_err() {
                    let _ = actions
                        .send_internal_cancellable(
                            RuntimeMessage::Action(Action::RuntimeDiagnostic {
                                category: DiagnosticCategory::State,
                                message: "player command failed".to_owned(),
                                media_id: None,
                            }),
                            &shutdown,
                        )
                        .await;
                }
            }
            OrderedPlayerEffect::Shutdown { started, completed } => {
                let _ = started.send(());
                if let Some(player) = &player
                    && player.shutdown().await.is_err()
                {
                    tracing::error!("player shutdown failed");
                }
                let _ = completed.send(());
                break;
            }
        }
    }
}

enum OrderedStorageEffect {
    SavePodcast(PodcastProgressCheckpoint),
    LoadPodcast {
        generation: Generation,
        video_id: String,
    },
    StoreChartCache {
        key: String,
        encoded: String,
        expires_at: i64,
        stored_at: i64,
    },
    RecordHistory(MediaItem),
    LoadHistory {
        generation: Generation,
        limit: usize,
    },
    LoadFavorites {
        generation: Generation,
    },
    AddFavorite {
        generation: Generation,
        item: MediaItem,
    },
    RemoveFavorite {
        generation: Generation,
        media_id: MediaId,
    },
    Shutdown(oneshot::Sender<()>),
}

struct OrderedStorageEffects {
    commands: mpsc::Sender<OrderedStorageEffect>,
    task: JoinHandle<()>,
}

impl OrderedStorageEffects {
    fn spawn(
        storage: Arc<dyn RuntimeStorage>,
        clock: Arc<dyn RuntimeClock>,
        actions: ActionSender,
        shutdown: CancellationToken,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(ORDERED_STORAGE_CAPACITY);
        let task = tokio::spawn(ordered_storage_loop(
            receiver, storage, clock, actions, shutdown,
        ));
        Self { commands, task }
    }

    async fn schedule(
        &self,
        effect: OrderedStorageEffect,
        shutdown: &CancellationToken,
    ) -> Result<(), OrderedStorageEffect> {
        let permit = tokio::select! {
            biased;
            () = shutdown.cancelled() => return Err(effect),
            result = self.commands.reserve() => match result {
                Ok(permit) => permit,
                Err(_) => return Err(effect),
            },
        };
        permit.send(effect);
        Ok(())
    }

    async fn shutdown(self, deadline: Instant) {
        let (completed, result) = oneshot::channel();
        let _ = tokio::time::timeout_at(
            deadline,
            self.commands
                .send(OrderedStorageEffect::Shutdown(completed)),
        )
        .await;
        if matches!(tokio::time::timeout_at(deadline, result).await, Ok(Ok(()))) {
            let _ = self.task.await;
        } else {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one FIFO match keeps storage effect ordering explicit and reviewable"
)]
async fn ordered_storage_loop(
    mut commands: mpsc::Receiver<OrderedStorageEffect>,
    storage: Arc<dyn RuntimeStorage>,
    clock: Arc<dyn RuntimeClock>,
    actions: ActionSender,
    shutdown: CancellationToken,
) {
    while let Some(effect) = commands.recv().await {
        match effect {
            OrderedStorageEffect::SavePodcast(checkpoint) => {
                let progress = PodcastProgress {
                    video_id: checkpoint.media_id().video_id.clone(),
                    playback_epoch: checkpoint.playback_epoch(),
                    position_ms: checkpoint.position_ms(),
                    duration_ms: checkpoint.duration_ms(),
                    played: checkpoint.played(),
                    updated_at: clock.now_millis(),
                };
                if storage.save_podcast_progress(progress).await.is_err() {
                    send_internal_runtime_diagnostic(
                        &actions,
                        &shutdown,
                        DiagnosticCategory::State,
                        "podcast progress persistence failed",
                    )
                    .await;
                }
            }
            OrderedStorageEffect::LoadPodcast {
                generation,
                video_id,
            } => {
                let progress = if let Ok(progress) = storage.load_podcast_progress(video_id).await {
                    progress
                } else {
                    send_internal_runtime_diagnostic(
                        &actions,
                        &shutdown,
                        DiagnosticCategory::State,
                        "podcast progress load failed",
                    )
                    .await;
                    None
                };
                let _ = actions
                    .send_internal_cancellable(
                        RuntimeMessage::Action(Action::PodcastProgressLoaded {
                            generation,
                            progress,
                        }),
                        &shutdown,
                    )
                    .await;
            }
            OrderedStorageEffect::StoreChartCache {
                key,
                encoded,
                expires_at,
                stored_at,
            } => {
                if storage
                    .store_chart_cache(key, encoded, expires_at, stored_at)
                    .await
                    .is_err()
                {
                    send_internal_runtime_diagnostic(
                        &actions,
                        &shutdown,
                        DiagnosticCategory::State,
                        "chart cache persistence failed",
                    )
                    .await;
                }
            }
            OrderedStorageEffect::RecordHistory(item) => {
                if storage
                    .record_history(item, clock.now_millis())
                    .await
                    .is_err()
                {
                    send_internal_runtime_diagnostic(
                        &actions,
                        &shutdown,
                        DiagnosticCategory::State,
                        "history persistence failed",
                    )
                    .await;
                }
            }
            OrderedStorageEffect::LoadHistory { generation, limit } => {
                let result = storage.load_history(limit).await.map_err(|_| {
                    AppError::new(AppErrorCategory::History, "history could not be loaded")
                });
                let _ = actions
                    .send_internal_cancellable(
                        RuntimeMessage::Action(Action::HistoryCompleted { generation, result }),
                        &shutdown,
                    )
                    .await;
            }
            OrderedStorageEffect::LoadFavorites { generation } => {
                let result = storage.load_favorites().await.map_err(|_| {
                    AppError::new(AppErrorCategory::Favorites, "favorites could not be loaded")
                });
                let _ = actions
                    .send_internal_cancellable(
                        RuntimeMessage::Action(Action::FavoritesCompleted { generation, result }),
                        &shutdown,
                    )
                    .await;
            }
            OrderedStorageEffect::AddFavorite { generation, item } => {
                let media_id = item.id.clone();
                let result = match storage.add_favorite(item, clock.now_millis()).await {
                    Ok(FavoriteInsertOutcome::Added | FavoriteInsertOutcome::AlreadyPresent) => {
                        storage.load_favorites().await.map_err(|_| {
                            AppError::new(
                                AppErrorCategory::Favorites,
                                "favorites could not be loaded",
                            )
                        })
                    }
                    Ok(FavoriteInsertOutcome::Full) => Err(AppError::new(
                        AppErrorCategory::Favorites,
                        "favorites are full; remove one before adding another",
                    )),
                    Err(_) => Err(AppError::new(
                        AppErrorCategory::Favorites,
                        "favorite could not be added",
                    )),
                };
                let _ = actions
                    .send_internal_cancellable(
                        RuntimeMessage::Action(Action::FavoriteMutationCompleted {
                            generation,
                            media_id,
                            mutation: FavoriteMutation::Add,
                            result,
                        }),
                        &shutdown,
                    )
                    .await;
            }
            OrderedStorageEffect::RemoveFavorite {
                generation,
                media_id,
            } => {
                let result = match storage.remove_favorite(media_id.clone()).await {
                    Ok(_) => storage.load_favorites().await.map_err(|_| {
                        AppError::new(AppErrorCategory::Favorites, "favorites could not be loaded")
                    }),
                    Err(_) => Err(AppError::new(
                        AppErrorCategory::Favorites,
                        "favorite could not be removed",
                    )),
                };
                let _ = actions
                    .send_internal_cancellable(
                        RuntimeMessage::Action(Action::FavoriteMutationCompleted {
                            generation,
                            media_id,
                            mutation: FavoriteMutation::Remove,
                            result,
                        }),
                        &shutdown,
                    )
                    .await;
            }
            OrderedStorageEffect::Shutdown(completed) => {
                let _ = completed.send(());
                break;
            }
        }
    }
}

#[derive(Default)]
struct LatestSessionSlot {
    checkpoint: Option<SessionCheckpoint>,
    closed: bool,
}

struct LatestSessionSender {
    slot: Arc<Mutex<LatestSessionSlot>>,
    changed: Arc<tokio::sync::Notify>,
}

impl LatestSessionSender {
    fn send(&self, checkpoint: SessionCheckpoint) {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.closed {
            return;
        }
        slot.checkpoint = Some(checkpoint);
        drop(slot);
        self.changed.notify_one();
    }

    fn close(&self) {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.closed = true;
        drop(slot);
        self.changed.notify_one();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        usize::from(
            self.slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .checkpoint
                .is_some(),
        )
    }
}

struct SessionPersister {
    commands: LatestSessionSender,
    task: JoinHandle<()>,
}

impl SessionPersister {
    fn spawn(storage: Arc<dyn RuntimeStorage>, clock: Arc<dyn RuntimeClock>) -> Self {
        let slot = Arc::new(Mutex::new(LatestSessionSlot::default()));
        let changed = Arc::new(tokio::sync::Notify::new());
        let commands = LatestSessionSender {
            slot: Arc::clone(&slot),
            changed: Arc::clone(&changed),
        };
        let task = tokio::spawn(session_persistence_loop(slot, changed, storage, clock));
        Self { commands, task }
    }

    fn schedule(&self, checkpoint: SessionCheckpoint) {
        self.commands.send(checkpoint);
    }

    async fn shutdown(self, deadline: Instant) {
        self.commands.close();
        let mut task = self.task;
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
}

async fn session_persistence_loop(
    slot: Arc<Mutex<LatestSessionSlot>>,
    changed: Arc<tokio::sync::Notify>,
    storage: Arc<dyn RuntimeStorage>,
    clock: Arc<dyn RuntimeClock>,
) {
    let mut pending: Option<SessionCheckpoint> = None;
    loop {
        let (next, closed) = take_latest_session(&slot);
        if let Some(checkpoint) = next {
            pending = Some(checkpoint);
        }
        if closed {
            save_pending_session(&storage, &clock, &mut pending).await;
            break;
        }
        if pending.is_none() {
            changed.notified().await;
            continue;
        }

        let deadline = Instant::now() + SESSION_DEBOUNCE;
        tokio::select! {
            biased;
            () = changed.notified() => {}
            () = tokio::time::sleep_until(deadline) => {
                save_pending_session(&storage, &clock, &mut pending).await;
            }
        }
    }
}

fn take_latest_session(slot: &Mutex<LatestSessionSlot>) -> (Option<SessionCheckpoint>, bool) {
    let mut slot = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (slot.checkpoint.take(), slot.closed)
}

async fn save_pending_session(
    storage: &Arc<dyn RuntimeStorage>,
    clock: &Arc<dyn RuntimeClock>,
    pending: &mut Option<SessionCheckpoint>,
) {
    let Some(checkpoint) = pending.take() else {
        return;
    };
    if storage
        .save_session(checkpoint, clock.now_millis())
        .await
        .is_err()
    {
        tracing::error!("session checkpoint persistence failed");
    }
}

async fn send_runtime_diagnostic(
    actions: &ActionSender,
    shutdown: &CancellationToken,
    category: DiagnosticCategory,
    message: &str,
) {
    let _ = actions
        .send_cancellable(
            RuntimeMessage::Action(Action::RuntimeDiagnostic {
                category,
                message: message.to_owned(),
                media_id: None,
            }),
            shutdown,
        )
        .await;
}

async fn send_internal_runtime_diagnostic(
    actions: &ActionSender,
    shutdown: &CancellationToken,
    category: DiagnosticCategory,
    message: &str,
) {
    let _ = actions
        .send_internal_cancellable(
            RuntimeMessage::Action(Action::RuntimeDiagnostic {
                category,
                message: message.to_owned(),
                media_id: None,
            }),
            shutdown,
        )
        .await;
}

fn unavailable_dependency_report() -> DoctorReport {
    DoctorReport::new(vec![
        DiagnosticRow::new(
            "browsing",
            DiagnosticStatus::Healthy,
            "metadata browsing available",
        ),
        DiagnosticRow::new(
            "playback",
            DiagnosticStatus::Unhealthy,
            "dependency checker unavailable",
        ),
    ])
}

fn provider_app_error(category: AppErrorCategory, error: ProviderError) -> AppError {
    AppError::new(category, error.to_string())
}

fn unavailable_boundary(category: AppErrorCategory) -> AppError {
    AppError::new(category, "runtime service is unavailable")
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error(transparent)]
    Storage(#[from] RuntimeStorageError),
    #[error(transparent)]
    Restore(#[from] RestoreError),
    #[error("runtime rendering failed")]
    Render(#[from] io::Error),
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit-test synchronization failures should retain their boundary-specific messages"
)]
mod bounded_runtime_tests {
    use super::*;

    #[test]
    fn ui_motion_spinner_demand_ignores_loading_content_hidden_by_an_overlay() {
        let state = crate::app::reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "hidden loading".to_owned(),
                filter: crate::app::SearchFilter::Songs,
            },
        )
        .0;
        let visible = RenderModel::default().with_view(crate::ui::render::NavigationItem::Search);
        assert!(state_motion_demand(&state, &visible).spinner);

        let hidden = visible.with_overlay(crate::ui::render::Overlay::Help);
        assert!(!state_motion_demand(&state, &hidden).spinner);
    }

    #[test]
    fn ui_motion_seek_tracking_uses_admitted_effects_and_retires_on_media_change() {
        let mut pending = false;
        update_pending_seek(
            &mut pending,
            &[Effect::Player(PlayerCommand::SeekRelative { seconds: 10 })],
            None,
        );
        assert!(pending);

        update_pending_seek(&mut pending, &[], Some(ProgressChange::Media));
        assert!(!pending);

        update_pending_seek(&mut pending, &[], None);
        assert!(
            !pending,
            "a rejected seek must not arm discontinuity motion"
        );
    }

    fn mouse_event(kind: crossterm::event::MouseEventKind) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column: 4,
            row: 7,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn crossterm_mouse_event_is_preserved_as_typed_runtime_input() {
        let mouse = mouse_event(crossterm::event::MouseEventKind::Down(
            crossterm::event::MouseButton::Left,
        ));
        assert_eq!(
            runtime_event_from_crossterm(&CrosstermEvent::Mouse(mouse)),
            Some(RuntimeEvent::Mouse(mouse))
        );
    }

    #[test]
    fn unsupported_crossterm_mouse_events_do_not_produce_runtime_input() {
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Up(crossterm::event::MouseButton::Left),
            MouseEventKind::Down(crossterm::event::MouseButton::Right),
            MouseEventKind::Down(crossterm::event::MouseButton::Middle),
            MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert_eq!(
                runtime_event_from_crossterm(&CrosstermEvent::Mouse(mouse_event(kind))),
                None,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn synchronous_controller_action_effects_are_collected_in_fifo_order() {
        let mut state = AppState::default();
        let effects = reduce_actions_and_collect_effects(
            &mut state,
            vec![Action::FavoritesRequested, Action::HistoryRequested],
        );

        assert!(matches!(
            effects.first(),
            Some(Effect::LoadFavorites { .. })
        ));
        assert!(matches!(effects.get(1), Some(Effect::LoadHistory { .. })));
        assert_eq!(effects.len(), 2);
    }

    #[test]
    fn production_service_composition_includes_favorites_startup() {
        let dependencies = DoctorReport::new(Vec::new());
        let services = RuntimeServices::new(Arc::new(NoopRuntimeStorage))
            .with_startup_actions(AuthenticationState::Authenticated, dependencies.clone());

        assert_eq!(
            services.initial_actions,
            startup_actions(AuthenticationState::Authenticated, dependencies)
        );
        assert!(
            services
                .initial_actions
                .contains(&Action::FavoritesRequested)
        );
    }

    #[test]
    fn crossterm_resize_is_preserved_as_a_distinct_runtime_event() {
        assert_eq!(
            runtime_event_from_crossterm(&CrosstermEvent::Resize(91, 27)),
            Some(RuntimeEvent::Resize(91, 27))
        );
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        assert!(matches!(
            classify_runtime_event(RuntimeEvent::Resize(91, 27), &terminal),
            RuntimeEventDisposition::Message(RuntimeMessage::Resize(91, 27))
        ));
    }

    #[test]
    fn mouse_events_classify_as_runtime_messages() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mouse = mouse_event(crossterm::event::MouseEventKind::ScrollDown);
        assert!(matches!(
            classify_runtime_event(RuntimeEvent::Mouse(mouse), &terminal),
            RuntimeEventDisposition::Message(RuntimeMessage::Mouse(actual)) if actual == mouse
        ));
    }

    #[test]
    fn mouse_only_adjacent_pointer_movements_coalesce_without_crossing_a_click() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        let click = mouse_event(crossterm::event::MouseEventKind::Down(
            crossterm::event::MouseButton::Left,
        ));
        let mut first_move = mouse_event(crossterm::event::MouseEventKind::Moved);
        first_move.column = 10;
        let mut latest_move = first_move;
        latest_move.column = 11;

        enqueue_pending_message(&mut pending, RuntimeMessage::Mouse(click), &terminal);
        enqueue_pending_message(&mut pending, RuntimeMessage::Mouse(first_move), &terminal);
        enqueue_pending_message(&mut pending, RuntimeMessage::Mouse(latest_move), &terminal);

        assert_eq!(
            pending,
            VecDeque::from([
                RuntimeMessage::Mouse(click),
                RuntimeMessage::Mouse(latest_move),
            ])
        );
    }

    #[test]
    fn pending_mouse_input_is_lossy_without_weakening_action_admission() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        for column in 0..EVENT_PENDING_CAPACITY {
            let kind = if column % 2 == 0 {
                crossterm::event::MouseEventKind::Moved
            } else {
                crossterm::event::MouseEventKind::ScrollDown
            };
            let mut mouse = mouse_event(kind);
            mouse.column = u16::try_from(column).expect("bounded column");
            enqueue_pending_message(&mut pending, RuntimeMessage::Mouse(mouse), &terminal);
        }
        let click = mouse_event(crossterm::event::MouseEventKind::Down(
            crossterm::event::MouseButton::Left,
        ));
        enqueue_pending_message(&mut pending, RuntimeMessage::Mouse(click), &terminal);
        assert_eq!(pending.len(), EVENT_PENDING_CAPACITY);
        assert!(
            pending
                .iter()
                .any(|message| message == &RuntimeMessage::Mouse(click)),
            "a queued movement should yield to the later click"
        );

        enqueue_pending_message(
            &mut pending,
            RuntimeMessage::Action(Action::TargetVolumeChanged(73)),
            &terminal,
        );
        assert_eq!(pending.len(), EVENT_PENDING_CAPACITY);
        assert!(pending.iter().any(|message| matches!(
            message,
            RuntimeMessage::Action(Action::TargetVolumeChanged(73))
        )));
    }

    #[test]
    fn saturated_mouse_input_keeps_terminal_control_polling_available() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        for column in 0..EVENT_PENDING_CAPACITY {
            let mut mouse = mouse_event(crossterm::event::MouseEventKind::ScrollDown);
            mouse.column = u16::try_from(column).expect("bounded column");
            enqueue_pending_message(&mut pending, RuntimeMessage::Mouse(mouse), &terminal);
        }
        assert!(pending_can_poll_event(&pending));
        assert!(matches!(
            classify_runtime_event(RuntimeEvent::Signal, &terminal),
            RuntimeEventDisposition::Terminal { panic: false }
        ));
    }

    #[test]
    fn saturated_mouse_queue_retains_resize_before_a_later_click() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut interactions = populated_interaction_store();
        let retained = interactions.latest().cloned().expect("published snapshot");
        let mut pending = VecDeque::new();
        for column in 0..EVENT_PENDING_CAPACITY {
            let kind = if column % 2 == 0 {
                MouseEventKind::Moved
            } else {
                MouseEventKind::ScrollDown
            };
            let mut mouse = mouse_event(kind);
            mouse.column = u16::try_from(column).expect("bounded column");
            enqueue_pending_message(&mut pending, RuntimeMessage::Mouse(mouse), &terminal);
        }

        enqueue_pending_message(&mut pending, RuntimeMessage::Resize(120, 40), &terminal);
        let click = mouse_event(MouseEventKind::Down(crossterm::event::MouseButton::Left));
        enqueue_pending_message(&mut pending, RuntimeMessage::Mouse(click), &terminal);

        let resize = pending
            .iter()
            .position(|message| matches!(message, RuntimeMessage::Resize(120, 40)))
            .expect("resize must replace lossy input");
        let click_index = pending
            .iter()
            .position(
                |message| matches!(message, RuntimeMessage::Mouse(actual) if *actual == click),
            )
            .expect("later click must replace pointer movement");
        assert!(resize < click_index);
        for message in pending.iter().skip(resize) {
            match message {
                RuntimeMessage::Resize(_, _) => interactions.invalidate(),
                RuntimeMessage::Mouse(actual) if *actual == click => {
                    assert_eq!(retained.resolve(0, 0), None);
                    break;
                }
                _ => {}
            }
        }
    }

    #[test]
    fn adjacent_resize_events_coalesce_to_latest_and_keep_terminal_polling_available() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        for width in 80..=120 {
            enqueue_pending_message(&mut pending, RuntimeMessage::Resize(width, 40), &terminal);
        }
        assert_eq!(pending, VecDeque::from([RuntimeMessage::Resize(120, 40)]));
        assert!(pending_can_poll_event(&pending));
    }

    #[test]
    fn resize_replacing_last_lossy_slot_preserves_terminal_and_lossless_action_admission() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        for volume in (0_u8..).take(EVENT_PENDING_CAPACITY - 1) {
            enqueue_pending_message(
                &mut pending,
                RuntimeMessage::Action(Action::TargetVolumeChanged(volume)),
                &terminal,
            );
        }
        enqueue_pending_message(
            &mut pending,
            RuntimeMessage::Mouse(mouse_event(MouseEventKind::ScrollDown)),
            &terminal,
        );
        enqueue_pending_message(&mut pending, RuntimeMessage::Resize(120, 40), &terminal);

        assert!(pending_can_poll_event(&pending));
        assert!(matches!(
            classify_runtime_event(RuntimeEvent::Signal, &terminal),
            RuntimeEventDisposition::Terminal { panic: false }
        ));

        enqueue_pending_message(
            &mut pending,
            RuntimeMessage::Action(Action::TargetVolumeChanged(99)),
            &terminal,
        );
        assert_eq!(pending.len(), EVENT_PENDING_CAPACITY);
        assert!(pending.iter().any(|message| matches!(
            message,
            RuntimeMessage::Action(Action::TargetVolumeChanged(99))
        )));
    }

    #[test]
    fn interaction_draw_lifecycle_invalidates_before_draw_and_publishes_after_success() {
        let mut store = populated_interaction_store();
        let retained = store.latest().cloned().expect("published snapshot");

        let result: io::Result<()> = draw_interaction_frame(&mut store, |map| {
            assert_eq!(retained.resolve(0, 0), None);
            let Some(map) = map else {
                panic!("revision available");
            };
            assert!(map.push(
                Rect::new(2, 2, 1, 1),
                crate::ui::interaction::HitTarget::Semantic(
                    crate::ui::input::SemanticAction::Submit,
                ),
            ));
            Ok(())
        });

        assert!(result.is_ok());
        assert!(store.latest().is_some_and(|snapshot| {
            snapshot.resolve(2, 2)
                == Some(crate::ui::interaction::HitTarget::Semantic(
                    crate::ui::input::SemanticAction::Submit,
                ))
        }));
    }

    #[test]
    fn interaction_draw_lifecycle_keeps_snapshot_empty_after_error() {
        let mut store = populated_interaction_store();
        let result = draw_interaction_frame(&mut store, |_map| {
            Err::<(), _>(io::Error::other("injected draw failure"))
        });
        assert!(result.is_err());
        assert!(store.latest().is_none());
    }

    #[test]
    fn interaction_draw_lifecycle_keeps_snapshot_empty_after_panic() {
        let mut store = populated_interaction_store();
        let panic = panic::catch_unwind(AssertUnwindSafe(|| {
            let _: io::Result<()> = draw_interaction_frame(&mut store, |_map| {
                panic!("injected draw panic");
            });
        }));
        assert!(panic.is_err());
        assert!(store.latest().is_none());
    }

    fn populated_interaction_store() -> InteractionStore {
        let mut store = InteractionStore::default();
        let mut map = store.begin_frame().expect("frame revision available");
        assert!(map.push(
            Rect::new(0, 0, 1, 1),
            crate::ui::interaction::HitTarget::Semantic(
                crate::ui::input::SemanticAction::TogglePlayback,
            ),
        ));
        assert!(store.publish(map));
        store
    }

    #[test]
    fn interaction_publication_exposes_only_latest_successful_frame() {
        let mut store = crate::ui::interaction::InteractionStore::default();
        let first = store.begin_frame().expect("first frame revision");
        let first_revision = first.revision();
        assert!(store.publish(first));
        assert_eq!(
            store.latest().map(InteractionSnapshot::revision),
            Some(first_revision)
        );

        let stale = crate::ui::interaction::InteractionMap::new(first_revision);
        let current = store.begin_frame().expect("next frame revision");
        assert!(store.latest().is_none());
        assert!(!store.publish(stale));
        assert!(store.publish(current));
        assert_ne!(
            store.latest().map(InteractionSnapshot::revision),
            Some(first_revision)
        );
    }

    #[test]
    fn renderer_interaction_snapshot_defaults_to_empty() {
        let renderer = VisualizerConfigurationRenderer::default();
        assert!(renderer.interaction_snapshot().is_none());
    }

    #[derive(Default)]
    struct VisualizerConfigurationRenderer {
        max_fps: Option<u8>,
    }

    impl Renderer for VisualizerConfigurationRenderer {
        fn configure_visualizer_max_fps(&mut self, max_fps: u8) {
            self.max_fps = Some(max_fps);
        }

        fn render(&mut self, _state: &AppState) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn runtime_renderer_receives_configured_effective_visualizer_rate() {
        let mut renderer = VisualizerConfigurationRenderer::default();
        configure_renderer_visualizer(&mut renderer, 7);
        assert_eq!(renderer.max_fps, Some(7));

        configure_renderer_visualizer(&mut renderer, 30);
        assert_eq!(renderer.max_fps, Some(30));
    }

    struct TerminalAfterPendingEventSource {
        events: VecDeque<RuntimeEvent>,
        terminal_observed: Option<oneshot::Sender<()>>,
    }

    #[async_trait]
    impl EventSource for TerminalAfterPendingEventSource {
        async fn next_event(&mut self) -> Option<RuntimeEvent> {
            let event = self.events.pop_front()?;
            if matches!(event, RuntimeEvent::Signal)
                && let Some(terminal_observed) = self.terminal_observed.take()
            {
                let _ = terminal_observed.send(());
            }
            Some(event)
        }
    }

    struct BlockingSessionStorage {
        started: mpsc::UnboundedSender<()>,
        release: Arc<Semaphore>,
    }

    struct NoopRuntimeStorage;

    #[async_trait]
    impl RuntimeStorage for NoopRuntimeStorage {
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
    }

    #[async_trait]
    impl RuntimeStorage for BlockingSessionStorage {
        async fn load_session(&self) -> Result<Option<SessionCheckpoint>, RuntimeStorageError> {
            Ok(None)
        }

        async fn save_session(
            &self,
            _checkpoint: SessionCheckpoint,
            _updated_at: i64,
        ) -> Result<(), RuntimeStorageError> {
            self.started.send(()).map_err(|_| RuntimeStorageError)?;
            self.release
                .acquire()
                .await
                .map_err(|_| RuntimeStorageError)?
                .forget();
            Ok(())
        }
    }

    #[test]
    fn pending_overload_preserves_every_lossless_action_in_fifo_order() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        for _ in 0..EVENT_PENDING_CAPACITY {
            enqueue_pending_message(
                &mut pending,
                RuntimeMessage::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
                &terminal,
            );
        }

        for volume in (0_u8..).take(EVENT_PENDING_CAPACITY) {
            assert!(
                pending_can_poll_event(&pending),
                "a lossy key should remain available for deterministic eviction"
            );
            enqueue_pending_message(
                &mut pending,
                RuntimeMessage::Action(Action::TargetVolumeChanged(volume)),
                &terminal,
            );
        }

        let retained = pending
            .iter()
            .map(|message| match message {
                RuntimeMessage::Action(Action::TargetVolumeChanged(volume)) => *volume,
                other => panic!("lossy key was retained ahead of a lossless action: {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            retained,
            (0_u8..).take(EVENT_PENDING_CAPACITY).collect::<Vec<_>>()
        );
        assert!(
            !pending_can_poll_event(&pending),
            "an all-lossless buffer must backpressure its source"
        );
    }

    #[test]
    fn pending_fifo_preserves_text_openers_and_q_before_overload() {
        for opener_code in [KeyCode::Char('/'), KeyCode::Char(':')] {
            let terminal = TerminalControlPlane::new(CancellationToken::new());
            let mut pending = VecDeque::new();
            let opener = KeyEvent::new(opener_code, KeyModifiers::NONE);
            let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
            enqueue_pending_message(&mut pending, RuntimeMessage::Key(opener), &terminal);
            assert!(
                matches!(
                    classify_runtime_event(RuntimeEvent::Key(q), &terminal),
                    RuntimeEventDisposition::Message(RuntimeMessage::Key(key)) if key == q
                ),
                "q must remain reducer input until {opener_code:?} is acknowledged"
            );
            enqueue_pending_message(&mut pending, RuntimeMessage::Key(q), &terminal);

            assert_eq!(
                pending.front(),
                Some(&RuntimeMessage::Key(opener)),
                "{opener_code:?} was reordered before overload"
            );
            assert_eq!(
                pending.get(1),
                Some(&RuntimeMessage::Key(q)),
                "text q was reordered after {opener_code:?}"
            );
        }
    }

    #[test]
    fn pending_overload_rolls_back_an_evicted_mode_prediction() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        let opener = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        enqueue_pending_message(&mut pending, RuntimeMessage::Key(opener), &terminal);
        for volume in (0_u8..).take(EVENT_PENDING_CAPACITY - 1) {
            enqueue_pending_message(
                &mut pending,
                RuntimeMessage::Action(Action::TargetVolumeChanged(volume)),
                &terminal,
            );
        }
        assert!(
            !terminal.key_requests_exit(q),
            "pending opener should conservatively classify q as text"
        );

        enqueue_pending_message(
            &mut pending,
            RuntimeMessage::Action(Action::TargetVolumeChanged(99)),
            &terminal,
        );

        assert!(
            terminal.key_requests_exit(q),
            "evicted opener left stale pending-mode state behind"
        );
        assert!(
            pending
                .iter()
                .all(|message| matches!(message, RuntimeMessage::Action(_))),
            "lossless action did not replace the evicted UI key"
        );
    }

    #[test]
    fn pending_overload_reprojects_after_evicted_trailing_transition() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        for code in [KeyCode::Char('/'), KeyCode::Enter, KeyCode::Char(':')] {
            enqueue_pending_message(
                &mut pending,
                RuntimeMessage::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                &terminal,
            );
        }
        for volume in (0_u8..).take(EVENT_PENDING_CAPACITY - 3) {
            enqueue_pending_message(
                &mut pending,
                RuntimeMessage::Action(Action::TargetVolumeChanged(volume)),
                &terminal,
            );
        }
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(
            !terminal.key_requests_exit(q),
            "trailing palette opener should project text entry before eviction"
        );

        enqueue_pending_message(
            &mut pending,
            RuntimeMessage::Action(Action::TargetVolumeChanged(99)),
            &terminal,
        );

        assert!(
            terminal.key_requests_exit(q),
            "evicting the trailing palette opener did not reproject / then Enter to Normal"
        );
    }

    #[test]
    fn pending_overload_reprojects_to_ambiguous_after_evicted_escape() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        for code in [KeyCode::Char(':'), KeyCode::Enter, KeyCode::Esc] {
            enqueue_pending_message(
                &mut pending,
                RuntimeMessage::Key(KeyEvent::new(code, KeyModifiers::NONE)),
                &terminal,
            );
        }
        for volume in (0_u8..).take(EVENT_PENDING_CAPACITY - 3) {
            enqueue_pending_message(
                &mut pending,
                RuntimeMessage::Action(Action::TargetVolumeChanged(volume)),
                &terminal,
            );
        }
        let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(
            terminal.key_requests_exit(q),
            "trailing Escape should definitively project Normal before eviction"
        );

        enqueue_pending_message(
            &mut pending,
            RuntimeMessage::Action(Action::TargetVolumeChanged(99)),
            &terminal,
        );

        assert!(
            !terminal.key_requests_exit(q),
            "evicting Escape did not restore the ambiguous palette Enter projection"
        );
    }

    #[test]
    fn pending_overload_coalesces_redraw_hints() {
        let terminal = TerminalControlPlane::new(CancellationToken::new());
        let mut pending = VecDeque::new();
        enqueue_pending_message(&mut pending, RuntimeMessage::Redraw, &terminal);
        enqueue_pending_message(&mut pending, RuntimeMessage::Redraw, &terminal);
        assert_eq!(pending, VecDeque::from([RuntimeMessage::Redraw]));
    }

    #[tokio::test]
    async fn closed_spectrum_redraw_receiver_is_disabled() {
        let (sender, receiver) = tokio::sync::watch::channel(0_u64);
        let mut redraw = Some(receiver);
        drop(sender);

        assert!(!redraw_receiver_changed(&mut redraw).await);
        assert!(redraw.is_none());
    }

    #[tokio::test]
    async fn terminal_cancellation_precedes_ready_spectrum_redraw() {
        let terminal = CancellationToken::new();
        let (_actions, mut action_rx) = bounded_action_channel(1);
        let (spectrum_tx, spectrum_rx) = tokio::sync::watch::channel(0_u64);
        let mut animation_redraw = None;
        let mut spectrum_redraw = Some(spectrum_rx);
        let mut ui_motion_redraw = None;
        spectrum_tx.send_replace(1);
        terminal.cancel();

        assert!(
            receive_runtime_message(
                &terminal,
                &mut action_rx,
                &mut animation_redraw,
                &mut spectrum_redraw,
                &mut ui_motion_redraw,
            )
            .await
            .is_none()
        );
    }

    #[tokio::test]
    async fn simultaneous_visual_redraws_coalesce_behind_queued_action() {
        let terminal = CancellationToken::new();
        let (actions, mut action_rx) = bounded_action_channel(1);
        let (animation_tx, animation_rx) = tokio::sync::watch::channel(0_u64);
        let (spectrum_tx, spectrum_rx) = tokio::sync::watch::channel(0_u64);
        let mut animation_redraw = Some(animation_rx);
        let mut spectrum_redraw = Some(spectrum_rx);
        let mut ui_motion_redraw = None;
        for revision in 1..=256 {
            animation_tx.send_replace(revision);
            spectrum_tx.send_replace(revision);
        }
        actions
            .send(RuntimeMessage::Action(Action::TargetVolumeChanged(37)))
            .await
            .expect("action channel open");

        let first = receive_runtime_message(
            &terminal,
            &mut action_rx,
            &mut animation_redraw,
            &mut spectrum_redraw,
            &mut ui_motion_redraw,
        )
        .await;
        assert_eq!(
            first,
            Some(RuntimeMessage::Action(Action::TargetVolumeChanged(37)))
        );
        for _ in 0..2 {
            assert_eq!(
                receive_runtime_message(
                    &terminal,
                    &mut action_rx,
                    &mut animation_redraw,
                    &mut spectrum_redraw,
                    &mut ui_motion_redraw,
                )
                .await,
                Some(RuntimeMessage::Redraw)
            );
        }
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                receive_runtime_message(
                    &terminal,
                    &mut action_rx,
                    &mut animation_redraw,
                    &mut spectrum_redraw,
                    &mut ui_motion_redraw,
                ),
            )
            .await
            .is_err(),
            "256 frames per source must coalesce to one redraw per receiver"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_action_is_published_before_terminal_cancellation_is_visible() {
        let (actions, _receiver) = bounded_action_channel(1);
        actions
            .send(RuntimeMessage::Redraw)
            .await
            .expect("first external permit should be available");
        let dispatch = CancellationToken::new();
        let terminal = TerminalControlPlane::new(dispatch);
        let shutdown = CancellationToken::new();
        let pending = RuntimeMessage::Action(Action::TargetVolumeChanged(37));
        let (terminal_observed_tx, terminal_observed_rx) = oneshot::channel();

        let locked_terminal = terminal.clone();
        let (lock_held_tx, lock_held_rx) = std::sync::mpsc::channel();
        let (release_lock_tx, release_lock_rx) = std::sync::mpsc::channel();
        let lock_holder = thread::spawn(move || {
            let _guard = locked_terminal
                .interrupted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock_held_tx
                .send(())
                .expect("test should observe held lock");
            release_lock_rx
                .recv()
                .expect("test should release retained-message lock");
        });
        lock_held_rx
            .recv()
            .expect("retained-message lock holder should start");

        let mut pump = tokio::spawn(pump_runtime_events(
            TerminalAfterPendingEventSource {
                events: VecDeque::from([
                    RuntimeEvent::Action(Action::TargetVolumeChanged(37)),
                    RuntimeEvent::Signal,
                ]),
                terminal_observed: Some(terminal_observed_tx),
            },
            actions,
            shutdown,
            terminal.clone(),
        ));
        terminal_observed_rx
            .await
            .expect("pump should poll the terminal event");
        let cancellation_visible =
            tokio::time::timeout(Duration::from_millis(20), terminal.first.cancelled())
                .await
                .is_ok();

        release_lock_tx
            .send(())
            .expect("retained-message lock holder should still be waiting");
        lock_holder
            .join()
            .expect("retained-message lock holder should not panic");
        tokio::time::timeout(Duration::from_millis(100), &mut pump)
            .await
            .expect("event pump should finish after publishing retained work")
            .expect("event pump task should not panic");

        assert!(
            !cancellation_visible,
            "terminal cancellation became visible before the pending action was published"
        );
        assert_eq!(terminal.take_interrupted(), VecDeque::from([pending]));
    }

    #[tokio::test(start_paused = true)]
    async fn session_persistence_retains_only_one_pending_update() {
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let release = Arc::new(Semaphore::new(0));
        let storage = Arc::new(BlockingSessionStorage {
            started: started_tx,
            release,
        });
        let persister = SessionPersister::spawn(storage, Arc::new(SystemRuntimeClock));
        let Some(checkpoint) = coherent_session_checkpoint(&AppState::default()) else {
            panic!("default state is coherent");
        };

        persister.schedule(checkpoint.clone());
        tokio::time::advance(SESSION_DEBOUNCE).await;
        let Some(()) = started_rx.recv().await else {
            panic!("first session save did not start");
        };
        for volume in 0..64 {
            let mut next = checkpoint.clone();
            next.playback.target_volume = volume;
            persister.schedule(next);
        }

        assert!(
            persister.commands.len() <= 1,
            "session persister retained more than one pending checkpoint"
        );
    }

    #[tokio::test]
    async fn completed_effect_tasks_retain_only_the_latest_owned_handle() {
        let storage: Arc<dyn RuntimeStorage> = Arc::new(NoopRuntimeStorage);
        let (actions, mut receiver) = bounded_action_channel(64);
        let shutdown = CancellationToken::new();
        let mut dispatcher = EffectDispatcher::new(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            storage,
            Arc::new(SystemRuntimeClock),
            actions,
            shutdown.clone(),
            shutdown,
        );

        for _ in 0..33 {
            dispatcher.dispatch(vec![Effect::CheckDependencies]).await;
            let Some(_) = receiver.recv().await else {
                panic!("dependency completion channel closed");
            };
        }
        dispatcher.dispatch(Vec::new()).await;

        assert!(
            dispatcher.dependencies_task.is_some(),
            "latest dependency task handle was not retained"
        );
        dispatcher
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
    }

    #[tokio::test]
    async fn podcast_recommendation_boundaries_publish_scoped_unavailable_completions() {
        let storage: Arc<dyn RuntimeStorage> = Arc::new(NoopRuntimeStorage);
        let (actions, mut receiver) = bounded_action_channel(4);
        let shutdown = CancellationToken::new();
        let mut dispatcher = EffectDispatcher::new(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            storage,
            Arc::new(SystemRuntimeClock),
            actions,
            shutdown.clone(),
            shutdown,
        );
        let source_generation = Generation::new(41);
        let resolve_generation = Generation::new(42);
        let region =
            RegionCode::parse("JP").unwrap_or_else(|error| panic!("valid test region: {error}"));
        let recommendation = crate::podcast_rankings::parse_apple_top_shows(
            br#"{"feed":{"country":"JP","results":[{"id":"source","name":"Sensitive title","artistName":"Sensitive publisher"}]}}"#,
        )
        .unwrap_or_else(|error| panic!("valid recommendation: {error}"))
        .items()[0]
            .clone();

        dispatcher
            .dispatch(vec![
                Effect::LoadPodcastRecommendations {
                    generation: source_generation,
                    region: region.clone(),
                },
                Effect::ResolvePodcastRecommendation {
                    generation: resolve_generation,
                    recommendation,
                },
            ])
            .await;

        let first = receiver
            .recv()
            .await
            .unwrap_or_else(|| panic!("first fallback completion channel closed"));
        let second = receiver
            .recv()
            .await
            .unwrap_or_else(|| panic!("second fallback completion channel closed"));
        let mut saw_source = false;
        let mut saw_match = false;
        for message in [first, second] {
            match message {
                RuntimeMessage::Action(Action::PodcastRecommendationsCompleted {
                    generation,
                    requested_region,
                    result: Err(error),
                }) => {
                    assert_eq!(generation, source_generation);
                    assert_eq!(requested_region, region);
                    assert_eq!(error.category(), AppErrorCategory::Podcast);
                    assert!(!error.message().contains("Sensitive"));
                    saw_source = true;
                }
                RuntimeMessage::Action(Action::PodcastRecommendationResolved {
                    generation,
                    result: Err(error),
                }) => {
                    assert_eq!(generation, resolve_generation);
                    assert_eq!(error.category(), AppErrorCategory::Podcast);
                    assert!(!error.message().contains("Sensitive"));
                    saw_match = true;
                }
                _ => panic!("unexpected podcast fallback message"),
            }
        }
        assert!(saw_source);
        assert!(saw_match);

        dispatcher
            .shutdown(Instant::now() + Duration::from_secs(1))
            .await;
    }

    #[tokio::test]
    async fn saturated_action_bus_does_not_block_podcast_fallback_dispatch() {
        let storage: Arc<dyn RuntimeStorage> = Arc::new(NoopRuntimeStorage);
        let (actions, _receiver) = bounded_action_channel(1);
        actions
            .send(RuntimeMessage::Redraw)
            .await
            .unwrap_or_else(|_| panic!("capacity-one bus must accept its first message"));
        let shutdown = CancellationToken::new();
        let mut dispatcher = EffectDispatcher::new(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            storage,
            Arc::new(SystemRuntimeClock),
            actions,
            shutdown.clone(),
            shutdown.clone(),
        );
        let region =
            RegionCode::parse("US").unwrap_or_else(|error| panic!("valid test region: {error}"));
        let recommendation = crate::podcast_rankings::parse_apple_top_shows(
            br#"{"feed":{"country":"US","results":[{"id":"source","name":"Show","artistName":"Publisher"}]}}"#,
        )
        .unwrap_or_else(|error| panic!("valid recommendation: {error}"))
        .items()[0]
            .clone();

        tokio::time::timeout(
            Duration::from_millis(100),
            dispatcher.dispatch(vec![
                Effect::LoadPodcastRecommendations {
                    generation: Generation::new(51),
                    region,
                },
                Effect::ResolvePodcastRecommendation {
                    generation: Generation::new(52),
                    recommendation,
                },
            ]),
        )
        .await
        .unwrap_or_else(|_| panic!("fallback dispatch blocked on the saturated action bus"));

        shutdown.cancel();
        tokio::time::timeout(
            Duration::from_secs(1),
            dispatcher.shutdown(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap_or_else(|_| panic!("terminal cancellation did not join fallback tasks"));
    }

    #[tokio::test]
    async fn terminal_cancellation_joins_blocked_podcast_fallback_tasks() {
        let storage: Arc<dyn RuntimeStorage> = Arc::new(NoopRuntimeStorage);
        let (actions, _receiver) = bounded_action_channel(1);
        actions
            .send(RuntimeMessage::Redraw)
            .await
            .unwrap_or_else(|_| panic!("capacity-one bus must accept its first message"));
        let shutdown = CancellationToken::new();
        let mut dispatcher = EffectDispatcher::new(
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            storage,
            Arc::new(SystemRuntimeClock),
            actions,
            shutdown.clone(),
            shutdown.clone(),
        );

        tokio::time::timeout(
            Duration::from_millis(100),
            dispatcher.dispatch(vec![Effect::LoadPodcastRecommendations {
                generation: Generation::new(61),
                region: RegionCode::parse("JP")
                    .unwrap_or_else(|error| panic!("valid test region: {error}")),
            }]),
        )
        .await
        .unwrap_or_else(|_| panic!("fallback task was not detached from dispatch"));

        shutdown.cancel();
        tokio::time::timeout(
            Duration::from_secs(1),
            dispatcher.shutdown(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap_or_else(|_| panic!("signal/quit cancellation did not finish fallback shutdown"));
    }
}
