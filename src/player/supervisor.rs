use std::{collections::VecDeque, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::{
    sync::{Mutex, mpsc, oneshot},
    task::{AbortHandle, JoinHandle},
    time::{Instant, Interval, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::{
    app::{Action, AppError, AppErrorCategory, FadeActivity, Generation, ResolverQuality},
    config::PlaybackConfig,
    domain::{MediaItem, PlaybackStatus},
    fade::{FadeCancel, FadeController, FadeDirection, FadeIntent, envelope_for_intent},
    resolver::{ResolvePolicy, ResolvedStream, Resolver},
};

use super::backend::{
    LoadEpoch, PlayerBackend, PlayerEndReason, PlayerError, PlayerErrorCategory, PlayerEvent,
};

const COMMAND_CAPACITY: usize = 32;
const ACTION_CAPACITY: usize = 64;
const RESOLUTION_CAPACITY: usize = 8;

#[async_trait]
pub trait TickSource: Send {
    async fn next_tick(&mut self) -> Option<Duration>;
}

pub struct TokioTickSource {
    interval: Interval,
    previous: Instant,
}

impl TokioTickSource {
    #[must_use]
    pub fn new(period: Duration) -> Self {
        let period = period.max(Duration::from_millis(1));
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        Self {
            interval,
            previous: Instant::now(),
        }
    }
}

#[async_trait]
impl TickSource for TokioTickSource {
    async fn next_tick(&mut self) -> Option<Duration> {
        let now = self.interval.tick().await;
        let delta = now.saturating_duration_since(self.previous);
        self.previous = now;
        Some(delta)
    }
}

pub struct PlayerSupervisor {
    commands: mpsc::Sender<Command>,
    actions: mpsc::Receiver<Action>,
    task: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct PlayerController {
    commands: mpsc::Sender<Command>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
    abort: Option<AbortHandle>,
}

pub struct PlayerActionStream {
    actions: mpsc::Receiver<Action>,
}

impl PlayerSupervisor {
    #[must_use]
    pub fn spawn(
        resolver: Arc<dyn Resolver>,
        backend: Box<dyn PlayerBackend>,
        config: PlaybackConfig,
        ticks: Box<dyn TickSource>,
    ) -> Self {
        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (action_tx, actions) = mpsc::channel(ACTION_CAPACITY);
        let actor = Actor::new(resolver, backend, config, ticks, command_rx, action_tx);
        let task = tokio::spawn(actor.run());
        Self {
            commands,
            actions,
            task: Some(task),
        }
    }

    /// Starts or replaces the active playback attempt.
    ///
    /// Resolution happens asynchronously. Completion and playback observations
    /// are returned as generation-tagged application actions.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor has already stopped.
    pub async fn play(
        &self,
        generation: Generation,
        item: MediaItem,
        start_ms: Option<u64>,
    ) -> Result<(), PlayerError> {
        self.request(|completed| Command::Play {
            generation,
            item: Box::new(item),
            start_ms,
            completed,
        })
        .await
    }

    /// Fades to silence before pausing.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor has already stopped.
    pub async fn pause(&self) -> Result<(), PlayerError> {
        self.request(Command::Pause).await
    }

    /// Unpauses at silence and starts the configured fade-in.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor has already stopped.
    pub async fn resume(&self) -> Result<(), PlayerError> {
        self.request(Command::Resume).await
    }

    /// Forwards a signed relative seek to the backend.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor has already stopped.
    pub async fn seek_relative(&self, seconds: i64) -> Result<(), PlayerError> {
        self.request(|completed| Command::SeekRelative { seconds, completed })
            .await
    }

    /// Forwards a playback multiplier to the backend.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor has already stopped.
    pub async fn set_speed(&self, speed: f64) -> Result<(), PlayerError> {
        self.request(|completed| Command::SetSpeed { speed, completed })
            .await
    }

    /// Changes the durable target volume while preserving an active fade.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor has already stopped.
    pub async fn set_volume(&self, volume: u8) -> Result<(), PlayerError> {
        self.request(|completed| Command::SetVolume { volume, completed })
            .await
    }

    #[must_use]
    pub async fn next_action(&mut self) -> Option<Action> {
        self.actions.recv().await
    }

    /// Separates the cloneable command boundary from the single-consumer
    /// application action stream.
    #[must_use]
    pub fn into_parts(mut self) -> (PlayerController, PlayerActionStream) {
        let task = self.task.take();
        let abort = task.as_ref().map(JoinHandle::abort_handle);
        (
            PlayerController {
                commands: self.commands,
                task: Arc::new(Mutex::new(task)),
                abort,
            },
            PlayerActionStream {
                actions: self.actions,
            },
        )
    }

    /// Stops the actor and asks the backend to shut down cleanly.
    ///
    /// # Errors
    ///
    /// Returns a backend shutdown error, or an error when the actor cannot be
    /// contacted or joined.
    pub async fn shutdown(mut self) -> Result<(), PlayerError> {
        let shutdown_result = self.request(Command::Shutdown).await;
        if let Some(task) = self.task.take() {
            task.await.map_err(|_| {
                PlayerError::new(
                    PlayerErrorCategory::Backend,
                    "player supervisor task stopped unexpectedly",
                )
            })?;
        }
        shutdown_result
    }

    async fn request(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<(), PlayerError>>) -> Command,
    ) -> Result<(), PlayerError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(build(completed))
            .await
            .map_err(|_| closed_error())?;
        result.await.map_err(|_| closed_error())?
    }
}

impl PlayerController {
    /// Starts one generation-tagged playback attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is stopped or playback setup fails.
    pub async fn play(
        &self,
        generation: Generation,
        item: MediaItem,
        start_ms: Option<u64>,
    ) -> Result<(), PlayerError> {
        self.request(|completed| Command::Play {
            generation,
            item: Box::new(item),
            start_ms,
            completed,
        })
        .await
    }

    /// Requests a fade-aware pause.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is stopped or the backend fails.
    pub async fn pause(&self) -> Result<(), PlayerError> {
        self.request(Command::Pause).await
    }

    /// Requests a fade-aware resume.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is stopped or the backend fails.
    pub async fn resume(&self) -> Result<(), PlayerError> {
        self.request(Command::Resume).await
    }

    /// Changes the target playback volume.
    ///
    /// # Errors
    ///
    /// Returns an error when the supervisor is stopped or the backend fails.
    pub async fn set_volume(&self, volume: u8) -> Result<(), PlayerError> {
        self.request(|completed| Command::SetVolume { volume, completed })
            .await
    }

    /// Stops the supervisor and joins its actor.
    ///
    /// # Errors
    ///
    /// Returns an error when shutdown cannot be requested, the backend fails,
    /// or the actor cannot be joined.
    pub async fn shutdown(&self) -> Result<(), PlayerError> {
        let shutdown_result = self.request(Command::Shutdown).await;
        if let Some(task) = self.task.lock().await.take() {
            task.await.map_err(|_| {
                PlayerError::new(
                    PlayerErrorCategory::Backend,
                    "player supervisor task stopped unexpectedly",
                )
            })?;
        }
        shutdown_result
    }

    pub fn abort(&self) {
        if let Some(abort) = &self.abort {
            abort.abort();
        }
    }

    async fn request(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<(), PlayerError>>) -> Command,
    ) -> Result<(), PlayerError> {
        let (completed, result) = oneshot::channel();
        self.commands
            .send(build(completed))
            .await
            .map_err(|_| closed_error())?;
        result.await.map_err(|_| closed_error())?
    }
}

impl PlayerActionStream {
    #[must_use]
    pub async fn next_action(&mut self) -> Option<Action> {
        self.actions.recv().await
    }
}

enum Command {
    Play {
        generation: Generation,
        item: Box<MediaItem>,
        start_ms: Option<u64>,
        completed: oneshot::Sender<Result<(), PlayerError>>,
    },
    Pause(oneshot::Sender<Result<(), PlayerError>>),
    Resume(oneshot::Sender<Result<(), PlayerError>>),
    SeekRelative {
        seconds: i64,
        completed: oneshot::Sender<Result<(), PlayerError>>,
    },
    SetSpeed {
        speed: f64,
        completed: oneshot::Sender<Result<(), PlayerError>>,
    },
    SetVolume {
        volume: u8,
        completed: oneshot::Sender<Result<(), PlayerError>>,
    },
    Shutdown(oneshot::Sender<Result<(), PlayerError>>),
}

struct ResolveCompletion {
    generation: Generation,
    result: Result<ResolvedStream, crate::resolver::ResolveError>,
}

struct PendingPlay {
    generation: Generation,
    item: MediaItem,
    start_ms: Option<u64>,
    policy: ResolvePolicy,
    starts_new_attempt: bool,
    result: Option<Result<ResolvedStream, crate::resolver::ResolveError>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LoadSubmissionId(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoadSubmissionState {
    Live,
    Tombstone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LoadSubmission {
    id: LoadSubmissionId,
    state: LoadSubmissionState,
}

#[derive(Clone)]
struct Current {
    pending_submission: Option<LoadSubmissionId>,
    epoch: Option<LoadEpoch>,
    generation: Generation,
    item: MediaItem,
    stream: ResolvedStream,
    start_ms: Option<u64>,
    natural_fade_started: bool,
}

/// Bounded overflow storage for actions while the application is not draining.
///
/// Each semantic class keeps only its newest value. Progress cannot evict
/// resolution, status, or terminal state, and the actor never waits for action
/// channel capacity while it is servicing playback commands.
#[derive(Default)]
struct PendingActions {
    resolution: Option<Action>,
    status: Option<Action>,
    terminal: Option<Action>,
    quality: Option<Action>,
    preview: Option<Action>,
    analysis: Option<Action>,
    telemetry: Option<Action>,
    progress: Option<Action>,
}

impl PendingActions {
    fn is_empty(&self) -> bool {
        self.resolution.is_none()
            && self.status.is_none()
            && self.terminal.is_none()
            && self.quality.is_none()
            && self.preview.is_none()
            && self.analysis.is_none()
            && self.telemetry.is_none()
            && self.progress.is_none()
    }

    fn push(&mut self, action: Action) {
        let slot = match action {
            Action::ResolveSucceeded { .. } | Action::ResolveFailed { .. } => &mut self.resolution,
            Action::PlayerStatusChanged { .. } => &mut self.status,
            Action::PlayerEnded { .. } => &mut self.terminal,
            Action::ResolvedFormatUpdated { .. } => &mut self.quality,
            Action::PreviewStreamUpdated { .. } => &mut self.preview,
            Action::AnalysisStreamUpdated { .. } => &mut self.analysis,
            Action::PlaybackTelemetryUpdated { .. } => &mut self.telemetry,
            Action::PlayerProgress { .. } => &mut self.progress,
            _ => unreachable!("player supervisor emitted a non-player action"),
        };
        *slot = Some(action);
    }

    fn pop(&mut self) -> Option<Action> {
        self.resolution
            .take()
            .or_else(|| self.status.take())
            .or_else(|| self.terminal.take())
            .or_else(|| self.quality.take())
            .or_else(|| self.preview.take())
            .or_else(|| self.analysis.take())
            .or_else(|| self.telemetry.take())
            .or_else(|| self.progress.take())
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transition {
    Pause,
    Replace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendRecovery {
    Polling { restart_used: bool },
    Quiescent,
}

impl BackendRecovery {
    const fn should_poll(self) -> bool {
        matches!(self, Self::Polling { .. })
    }
}

struct Actor {
    resolver: Arc<dyn Resolver>,
    backend: Box<dyn PlayerBackend>,
    config: PlaybackConfig,
    ticks: Box<dyn TickSource>,
    commands: mpsc::Receiver<Command>,
    actions: Option<mpsc::Sender<Action>>,
    pending_actions: PendingActions,
    resolution_tx: mpsc::Sender<Box<ResolveCompletion>>,
    resolutions: mpsc::Receiver<Box<ResolveCompletion>>,
    resolve_cancel: Option<CancellationToken>,
    pending: Option<PendingPlay>,
    current: Option<Current>,
    staging: Option<Current>,
    load_submissions: VecDeque<LoadSubmission>,
    next_load_submission: Option<u64>,
    last_epoch: Option<LoadEpoch>,
    fade: FadeController,
    transition: Option<Transition>,
    ticks_open: bool,
    backend_recovery: BackendRecovery,
    url_refresh_used: bool,
    desired_paused: bool,
}

impl Actor {
    fn new(
        resolver: Arc<dyn Resolver>,
        backend: Box<dyn PlayerBackend>,
        config: PlaybackConfig,
        ticks: Box<dyn TickSource>,
        commands: mpsc::Receiver<Command>,
        actions: mpsc::Sender<Action>,
    ) -> Self {
        let (resolution_tx, resolutions) = mpsc::channel(RESOLUTION_CAPACITY);
        Self {
            resolver,
            backend,
            fade: FadeController::new(f64::from(config.volume)),
            config,
            ticks,
            commands,
            actions: Some(actions),
            pending_actions: PendingActions::default(),
            resolution_tx,
            resolutions,
            resolve_cancel: None,
            pending: None,
            current: None,
            staging: None,
            load_submissions: VecDeque::new(),
            next_load_submission: Some(1),
            last_epoch: None,
            transition: None,
            ticks_open: true,
            backend_recovery: BackendRecovery::Polling {
                restart_used: false,
            },
            url_refresh_used: false,
            desired_paused: false,
        }
    }

    async fn run(mut self) {
        loop {
            let poll_backend = self.backend_recovery.should_poll();
            let flush_actions = !self.pending_actions.is_empty() && self.actions.is_some();
            let action_sender = self.actions.clone();
            let input = tokio::select! {
                biased;
                command = self.commands.recv() => Input::Command(command),
                permit = async move {
                    let action_sender = action_sender?;
                    action_sender.reserve_owned().await.ok()
                }, if flush_actions => Input::ActionPermit(permit),
                completion = self.resolutions.recv() => Input::Resolution(completion),
                tick = self.ticks.next_tick(), if self.ticks_open => Input::Tick(tick),
                event = self.backend.next_event(), if poll_backend => {
                    Input::Backend(event)
                }
            };

            let keep_running = match input {
                Input::Command(Some(command)) => self.handle_command(command).await,
                Input::Command(None) => false,
                Input::ActionPermit(Some(permit)) => {
                    if let Some(action) = self.pending_actions.pop() {
                        permit.send(action);
                    }
                    true
                }
                Input::ActionPermit(None) => {
                    self.actions = None;
                    self.pending_actions.clear();
                    true
                }
                Input::Resolution(Some(completion)) => {
                    self.handle_resolution(*completion).await;
                    true
                }
                Input::Resolution(None) => true,
                Input::Tick(Some(delta)) => {
                    self.handle_tick(delta).await;
                    true
                }
                Input::Tick(None) => {
                    self.ticks_open = false;
                    true
                }
                Input::Backend(event) => {
                    self.handle_backend_event(event).await;
                    true
                }
            };
            if !keep_running {
                break;
            }
        }
        if let Some(cancel) = self.resolve_cancel.take() {
            cancel.cancel();
        }
    }

    async fn handle_command(&mut self, command: Command) -> bool {
        match command {
            Command::Play {
                generation,
                item,
                start_ms,
                completed,
            } => {
                self.begin_play(generation, *item, start_ms).await;
                let _ = completed.send(Ok(()));
                true
            }
            Command::Pause(completed) => {
                self.begin_pause().await;
                let _ = completed.send(Ok(()));
                true
            }
            Command::Resume(completed) => {
                self.desired_paused = false;
                let deferred = self.transition == Some(Transition::Replace)
                    || self.current.is_none() && self.staging.is_none();
                let result = if deferred {
                    Ok(())
                } else {
                    self.resume_current().await
                };
                if result.is_err() {
                    self.emit_failed_status();
                } else if !deferred {
                    self.emit_status(PlaybackStatus::Playing);
                }
                let _ = completed.send(result);
                true
            }
            Command::SeekRelative { seconds, completed } => {
                let result = self.backend.seek_relative(seconds).await;
                if result.is_err() {
                    self.emit_failed_status();
                }
                let _ = completed.send(result);
                true
            }
            Command::SetSpeed { speed, completed } => {
                let result = self.backend.set_speed(speed).await;
                if result.is_err() {
                    self.emit_failed_status();
                }
                let _ = completed.send(result);
                true
            }
            Command::SetVolume { volume, completed } => {
                self.fade.set_target_volume(f64::from(volume));
                let result = self.backend.set_volume(self.fade.effective_volume()).await;
                self.emit_telemetry();
                let _ = completed.send(result);
                true
            }
            Command::Shutdown(completed) => {
                if let Some(cancel) = self.resolve_cancel.take() {
                    cancel.cancel();
                }
                let result = self.backend.shutdown().await;
                let _ = completed.send(result);
                false
            }
        }
    }

    async fn begin_play(&mut self, generation: Generation, item: MediaItem, start_ms: Option<u64>) {
        if let Some(cancel) = self.resolve_cancel.take() {
            cancel.cancel();
        }
        self.tombstone_playback_submissions();
        self.desired_paused = false;
        let replacing_playback = self.current.is_some() || self.staging.is_some();

        let starts_new_attempt = self
            .current
            .as_ref()
            .is_none_or(|current| current.generation != generation)
            && self
                .staging
                .as_ref()
                .is_none_or(|staging| staging.generation != generation)
            && self
                .pending
                .as_ref()
                .is_none_or(|pending| pending.generation != generation);
        if starts_new_attempt {
            self.backend_recovery = BackendRecovery::Quiescent;
        }
        self.pending = Some(PendingPlay {
            generation,
            item: item.clone(),
            start_ms,
            policy: ResolvePolicy::UseCache,
            starts_new_attempt,
            result: None,
        });
        self.staging = None;
        self.spawn_resolution(generation, item, ResolvePolicy::UseCache);

        if replacing_playback {
            self.fade.start_envelope(envelope_for_intent(
                FadeIntent::Replace,
                &self.config,
                self.fade.effective_volume(),
            ));
            self.transition = Some(Transition::Replace);
            if !self.fade.is_active() {
                let _ = self.backend.set_volume(0.0).await;
            }
            self.emit_telemetry();
        } else {
            self.fade.cancel(FadeCancel::Silence);
            self.transition = None;
        }
        self.url_refresh_used = false;
    }

    fn spawn_resolution(&mut self, generation: Generation, item: MediaItem, policy: ResolvePolicy) {
        let cancel = CancellationToken::new();
        self.resolve_cancel = Some(cancel.clone());
        let resolver = self.resolver.clone();
        let completed = self.resolution_tx.clone();
        let _resolver_task = tokio::spawn(async move {
            let result = resolver
                .resolve_with_policy(&item, None, policy, cancel)
                .await;
            let _ = completed
                .send(Box::new(ResolveCompletion { generation, result }))
                .await;
        });
    }

    async fn handle_resolution(&mut self, completion: ResolveCompletion) {
        let Some(pending) = self.pending.as_mut() else {
            return;
        };
        if pending.generation != completion.generation {
            return;
        }
        pending.result = Some(completion.result);
        self.resolve_cancel = None;

        if self.transition != Some(Transition::Replace) || !self.fade.is_active() {
            self.load_pending().await;
        }
    }

    async fn load_pending(&mut self) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        let Some(result) = pending.result.take() else {
            self.pending = Some(pending);
            return;
        };

        match result {
            Ok(stream) => {
                let quality = ResolverQuality::from_resolved_stream(&stream);
                let preview_url = stream.preview_url.clone();
                let analysis_url = stream.analysis_stream_url();
                let silence_result = if self.transition == Some(Transition::Replace) {
                    Ok(())
                } else {
                    self.backend.set_volume(0.0).await
                };
                if silence_result.is_err() {
                    self.emit_resolve_failed(
                        pending.generation,
                        "the playback backend rejected the resolved stream",
                    );
                    return;
                }
                let Ok(submission) = self
                    .submit_backend_load(&stream.url, pending.start_ms)
                    .await
                else {
                    self.emit_resolve_failed(
                        pending.generation,
                        "the playback backend rejected the resolved stream",
                    );
                    return;
                };
                if self.desired_paused
                    && let Err(error) = self.backend.set_paused(true).await
                {
                    self.handle_post_load_control_failure(submission, &error)
                        .await;
                    self.emit_resolve_failed(
                        pending.generation,
                        "the playback backend rejected the resolved stream",
                    );
                    return;
                }
                self.fade.cancel(FadeCancel::Silence);
                if !self.desired_paused {
                    self.fade.start_envelope(envelope_for_intent(
                        FadeIntent::Play,
                        &self.config,
                        0.0,
                    ));
                }
                self.staging = Some(Current {
                    pending_submission: Some(submission),
                    epoch: None,
                    generation: pending.generation,
                    item: pending.item,
                    stream,
                    start_ms: pending.start_ms,
                    natural_fade_started: false,
                });
                self.transition = None;
                if pending.starts_new_attempt {
                    self.backend_recovery = BackendRecovery::Polling {
                        restart_used: false,
                    };
                }
                self.url_refresh_used = pending.policy == ResolvePolicy::ForceRefresh;
                if !self.desired_paused {
                    self.apply_immediate_fade_endpoint().await;
                }
                self.emit_action(Action::ResolveSucceeded {
                    generation: pending.generation,
                });
                self.emit_action(Action::ResolvedFormatUpdated {
                    generation: pending.generation,
                    quality,
                });
                self.emit_action(Action::PreviewStreamUpdated {
                    generation: pending.generation,
                    preview_url,
                });
                self.emit_action(Action::AnalysisStreamUpdated {
                    generation: pending.generation,
                    stream_url: analysis_url,
                });
                self.emit_telemetry();
            }
            Err(error) => {
                self.emit_resolve_failed(pending.generation, &error.to_string());
            }
        }
    }

    async fn begin_pause(&mut self) {
        self.desired_paused = true;
        if self.transition == Some(Transition::Replace)
            || self.current.is_none() && self.staging.is_none()
        {
            return;
        }
        if self.fade.effective_volume() == 0.0 {
            self.fade.cancel(FadeCancel::Silence);
            self.transition = Some(Transition::Pause);
            let _ = self.backend.set_volume(0.0).await;
            self.emit_telemetry();
            self.complete_transition().await;
            return;
        }
        self.fade.start_envelope(envelope_for_intent(
            FadeIntent::Pause,
            &self.config,
            self.fade.effective_volume(),
        ));
        self.transition = Some(Transition::Pause);
        self.emit_telemetry();
        if !self.fade.is_active() {
            let _ = self.backend.set_volume(0.0).await;
            self.complete_transition().await;
        }
    }

    async fn resume_current(&mut self) -> Result<(), PlayerError> {
        self.transition = None;
        self.backend.set_paused(false).await?;
        self.fade.cancel(FadeCancel::Silence);
        self.backend.set_volume(0.0).await?;
        self.fade
            .start_envelope(envelope_for_intent(FadeIntent::Resume, &self.config, 0.0));
        self.apply_immediate_fade_endpoint().await;
        self.emit_telemetry();
        Ok(())
    }

    async fn handle_tick(&mut self, delta: Duration) {
        if !self.fade.is_active() {
            return;
        }
        let volume = self.fade.tick(delta);
        if self.backend.set_volume(volume).await.is_err() {
            self.emit_failed_status();
            return;
        }
        self.emit_telemetry();
        if !self.fade.is_active() {
            self.complete_transition().await;
        }
    }

    async fn complete_transition(&mut self) {
        match self.transition {
            Some(Transition::Pause) => {
                self.transition = None;
                if self.backend.set_paused(true).await.is_ok() {
                    self.emit_status(PlaybackStatus::Paused);
                } else {
                    self.emit_failed_status();
                }
            }
            Some(Transition::Replace) => {
                self.load_pending().await;
            }
            None => {}
        }
    }

    async fn apply_immediate_fade_endpoint(&mut self) {
        if !self.fade.is_active() {
            let _ = self.backend.set_volume(self.fade.effective_volume()).await;
        }
    }

    async fn handle_backend_event(&mut self, event: Result<PlayerEvent, PlayerError>) {
        let Ok(event) = event else {
            self.handle_backend_shutdown().await;
            return;
        };
        match event {
            PlayerEvent::LoadStarted { epoch } => self.bind_load_epoch(epoch),
            PlayerEvent::FileLoaded { epoch } => {
                let staged_matches = self
                    .staging
                    .as_ref()
                    .is_some_and(|staging| staging.epoch == Some(epoch));
                if staged_matches {
                    self.current = self.staging.take();
                } else if self.staging.is_some()
                    || self
                        .current
                        .as_ref()
                        .is_none_or(|current| current.epoch != Some(epoch))
                {
                    return;
                }
                self.emit_status(if self.desired_paused {
                    PlaybackStatus::Paused
                } else {
                    PlaybackStatus::Playing
                });
            }
            PlayerEvent::Progress {
                epoch,
                position_ms,
                duration_ms,
            } => {
                let (generation, media_id, begin_natural_fade) = {
                    if self.staging.is_some() {
                        return;
                    }
                    let Some(current) = self
                        .current
                        .as_mut()
                        .filter(|current| current.epoch == Some(epoch))
                    else {
                        return;
                    };
                    current.start_ms = Some(position_ms);
                    let begin_natural_fade = !current.natural_fade_started
                        && duration_ms.is_some_and(|duration_ms| {
                            duration_ms.saturating_sub(position_ms) <= self.config.fade_out_ms
                        });
                    if begin_natural_fade {
                        current.natural_fade_started = true;
                    }
                    (
                        current.generation,
                        current.item.id.clone(),
                        begin_natural_fade,
                    )
                };
                if begin_natural_fade {
                    self.fade.start_envelope(envelope_for_intent(
                        FadeIntent::NaturalEnd,
                        &self.config,
                        self.fade.effective_volume(),
                    ));
                    self.apply_immediate_fade_endpoint().await;
                    self.emit_telemetry();
                }
                self.emit_action(Action::PlayerProgress {
                    generation,
                    media_id,
                    position_ms,
                    duration_ms,
                });
            }
            PlayerEvent::PauseChanged { epoch, paused } => {
                if self.staging.is_some()
                    || self
                        .current
                        .as_ref()
                        .is_none_or(|current| current.epoch != Some(epoch))
                {
                    return;
                }
                self.emit_status(if paused {
                    PlaybackStatus::Paused
                } else {
                    PlaybackStatus::Playing
                });
            }
            PlayerEvent::VolumeChanged(_) | PlayerEvent::SpeedChanged(_) => {}
            PlayerEvent::Ended { epoch, reason } => self.handle_end(epoch, reason),
            PlayerEvent::Shutdown => self.handle_backend_shutdown().await,
        }
    }

    fn bind_load_epoch(&mut self, epoch: LoadEpoch) {
        if self.last_epoch.is_some_and(|last| epoch <= last) {
            return;
        }
        self.last_epoch = Some(epoch);
        let Some(submission) = self.load_submissions.pop_front() else {
            return;
        };
        if submission.state == LoadSubmissionState::Tombstone {
            return;
        }
        if let Some(staging) = self.staging.as_mut().filter(|staging| {
            staging.pending_submission == Some(submission.id) && staging.epoch.is_none()
        }) {
            staging.pending_submission = None;
            staging.epoch = Some(epoch);
            return;
        }
        if let Some(current) = self.current.as_mut().filter(|current| {
            current.pending_submission == Some(submission.id) && current.epoch.is_none()
        }) {
            current.pending_submission = None;
            current.epoch = Some(epoch);
        }
    }

    fn handle_end(&mut self, epoch: LoadEpoch, reason: PlayerEndReason) {
        let (playback, staged) = if let Some(staging) = self
            .staging
            .as_ref()
            .filter(|staging| staging.epoch == Some(epoch))
        {
            (staging.clone(), true)
        } else if self.staging.is_some() {
            return;
        } else if let Some(current) = self
            .current
            .as_ref()
            .filter(|current| current.epoch == Some(epoch))
        {
            (current.clone(), false)
        } else {
            return;
        };
        if staged && reason != PlayerEndReason::UrlRejected {
            return;
        }
        match reason {
            PlayerEndReason::Natural => {
                self.emit_action(Action::PlayerEnded {
                    generation: playback.generation,
                });
            }
            PlayerEndReason::UrlRejected if !self.url_refresh_used => {
                self.url_refresh_used = true;
                if staged && let Some(staging) = self.staging.as_mut() {
                    staging.epoch = None;
                }
                let generation = playback.generation;
                let item = playback.item;
                let start_ms = playback.start_ms;
                self.pending = Some(PendingPlay {
                    generation,
                    item: item.clone(),
                    start_ms,
                    policy: ResolvePolicy::ForceRefresh,
                    starts_new_attempt: false,
                    result: None,
                });
                self.transition = None;
                self.spawn_resolution(generation, item, ResolvePolicy::ForceRefresh);
            }
            PlayerEndReason::UrlRejected => {
                if let Some(cancel) = self.resolve_cancel.take() {
                    cancel.cancel();
                }
                self.pending = None;
                self.transition = None;
                self.backend_recovery = BackendRecovery::Quiescent;
                let generation = playback.generation;
                self.emit_status_for(generation, PlaybackStatus::Failed);
            }
            PlayerEndReason::Replaced | PlayerEndReason::Stopped | PlayerEndReason::Unknown => {}
        }
    }

    async fn handle_backend_shutdown(&mut self) {
        self.clear_load_submissions();
        let restarting_staging = self.staging.is_some();
        let Some(playback) = self.staging.as_ref().or(self.current.as_ref()) else {
            self.backend_recovery = BackendRecovery::Quiescent;
            return;
        };
        let generation = playback.generation;
        let url = playback.stream.url.clone();
        let start_ms = playback.start_ms;
        match self.backend_recovery {
            BackendRecovery::Quiescent => return,
            BackendRecovery::Polling { restart_used: true } => {
                self.backend_recovery = BackendRecovery::Quiescent;
                self.emit_status_for(generation, PlaybackStatus::Failed);
                return;
            }
            BackendRecovery::Polling {
                restart_used: false,
            } => {
                self.backend_recovery = BackendRecovery::Polling { restart_used: true };
            }
        }
        if self.backend.set_volume(0.0).await.is_err() {
            self.backend_recovery = BackendRecovery::Quiescent;
            self.emit_status_for(generation, PlaybackStatus::Failed);
            return;
        }
        let Ok(submission) = self.submit_backend_load(&url, start_ms).await else {
            self.backend_recovery = BackendRecovery::Quiescent;
            self.emit_status_for(generation, PlaybackStatus::Failed);
            return;
        };
        if self.desired_paused
            && let Err(error) = self.backend.set_paused(true).await
        {
            self.handle_post_load_control_failure(submission, &error)
                .await;
            self.backend_recovery = BackendRecovery::Quiescent;
            self.emit_status_for(generation, PlaybackStatus::Failed);
            return;
        }
        self.fade.cancel(FadeCancel::Silence);
        self.transition = None;
        if restarting_staging {
            if let Some(staging) = self.staging.as_mut() {
                staging.pending_submission = Some(submission);
                staging.epoch = None;
            }
        } else if let Some(current) = self.current.as_mut() {
            current.pending_submission = Some(submission);
            current.epoch = None;
        }
        if !self.desired_paused {
            self.fade
                .start_envelope(envelope_for_intent(FadeIntent::Play, &self.config, 0.0));
            self.apply_immediate_fade_endpoint().await;
        }
        self.emit_telemetry();
    }

    async fn submit_backend_load(
        &mut self,
        url: &url::Url,
        start_ms: Option<u64>,
    ) -> Result<LoadSubmissionId, PlayerError> {
        let submission = self.register_load_submission()?;
        if let Err(error) = self.backend.load(url, start_ms).await {
            self.clear_load_submissions();
            return Err(error);
        }
        Ok(submission)
    }

    async fn handle_post_load_control_failure(
        &mut self,
        submission: LoadSubmissionId,
        error: &PlayerError,
    ) {
        if matches!(
            error.category(),
            PlayerErrorCategory::Closed
                | PlayerErrorCategory::Connection
                | PlayerErrorCategory::Protocol
                | PlayerErrorCategory::Spawn
        ) {
            self.backend.reset_session().await;
            self.clear_load_submissions();
            self.backend_recovery = BackendRecovery::Quiescent;
        } else {
            self.tombstone_submission(submission);
        }
    }

    fn register_load_submission(&mut self) -> Result<LoadSubmissionId, PlayerError> {
        let value = self.next_load_submission.ok_or_else(|| {
            PlayerError::new(
                PlayerErrorCategory::Backend,
                "playback load submission space is exhausted",
            )
        })?;
        self.next_load_submission = value.checked_add(1);
        let id = LoadSubmissionId(value);
        self.load_submissions.push_back(LoadSubmission {
            id,
            state: LoadSubmissionState::Live,
        });
        Ok(id)
    }

    fn tombstone_submission(&mut self, id: LoadSubmissionId) {
        if let Some(submission) = self
            .load_submissions
            .iter_mut()
            .find(|submission| submission.id == id)
        {
            submission.state = LoadSubmissionState::Tombstone;
        }
    }

    fn tombstone_playback_submissions(&mut self) {
        let current = self
            .current
            .as_mut()
            .and_then(|current| current.pending_submission.take());
        let staging = self
            .staging
            .as_mut()
            .and_then(|staging| staging.pending_submission.take());
        for submission in [current, staging].into_iter().flatten() {
            self.tombstone_submission(submission);
        }
    }

    fn clear_load_submissions(&mut self) {
        self.load_submissions.clear();
        if let Some(current) = self.current.as_mut() {
            current.pending_submission = None;
        }
        if let Some(staging) = self.staging.as_mut() {
            staging.pending_submission = None;
        }
    }

    fn emit_action(&mut self, action: Action) {
        if !self.pending_actions.is_empty() {
            self.pending_actions.push(action);
            return;
        }
        let Some(actions) = self.actions.as_ref() else {
            return;
        };
        match actions.try_send(action) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(action)) => self.pending_actions.push(action),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.actions = None;
                self.pending_actions.clear();
            }
        }
    }

    fn emit_resolve_failed(&mut self, generation: Generation, message: &str) {
        self.emit_action(Action::ResolveFailed {
            generation,
            error: AppError::new(AppErrorCategory::Resolve, message),
        });
    }

    fn emit_status(&mut self, status: PlaybackStatus) {
        if self.staging.is_some() {
            return;
        }
        if let Some(current) = self.current.as_ref() {
            self.emit_status_for(current.generation, status);
        }
    }

    fn emit_status_for(&mut self, generation: Generation, status: PlaybackStatus) {
        self.emit_action(Action::PlayerStatusChanged { generation, status });
    }

    fn emit_failed_status(&mut self) {
        self.emit_status(PlaybackStatus::Failed);
    }

    fn emit_telemetry(&mut self) {
        let Some(generation) = self.telemetry_generation() else {
            return;
        };
        let fade = match self.fade.direction() {
            Some(FadeDirection::In) => Some(FadeActivity::In),
            Some(FadeDirection::Out) => Some(FadeActivity::Out),
            None => None,
        };
        self.emit_action(Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: self.fade.effective_volume(),
            fade,
        });
    }

    fn telemetry_generation(&self) -> Option<Generation> {
        self.pending
            .as_ref()
            .map(|pending| pending.generation)
            .or_else(|| self.staging.as_ref().map(|staging| staging.generation))
            .or_else(|| self.current.as_ref().map(|current| current.generation))
    }
}

enum Input {
    Command(Option<Command>),
    ActionPermit(Option<mpsc::OwnedPermit<Action>>),
    Resolution(Option<Box<ResolveCompletion>>),
    Tick(Option<Duration>),
    Backend(Result<PlayerEvent, PlayerError>),
}

fn closed_error() -> PlayerError {
    PlayerError::new(
        PlayerErrorCategory::Closed,
        "player supervisor is not running",
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{FadeActivity, ResolverQuality},
        domain::MediaId,
        resolver::{PreviewStreamUrl, ResolvedStream},
    };

    use super::*;

    #[test]
    fn pending_actions_protect_quality_and_follow_the_documented_priority() {
        let generation = Generation::new(9);
        let media_id = MediaId {
            provider: "youtube-music".to_owned(),
            video_id: "pending".to_owned(),
        };
        let mut pending = PendingActions::default();
        pending.push(Action::PlayerProgress {
            generation,
            media_id: media_id.clone(),
            position_ms: 10,
            duration_ms: Some(20),
        });
        pending.push(Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 50.0,
            fade: Some(FadeActivity::In),
        });
        pending.push(Action::ResolvedFormatUpdated {
            generation,
            quality: ResolverQuality::new(Some("opus"), Some("251")),
        });
        pending.push(Action::PreviewStreamUpdated {
            generation,
            preview_url: Some(
                PreviewStreamUrl::parse("https://video.invalid/pending")
                    .unwrap_or_else(|error| panic!("test preview URL should parse: {error}")),
            ),
        });
        let analysis_stream = ResolvedStream::new(
            media_id.clone(),
            url::Url::parse("https://media.invalid/first-analysis")
                .unwrap_or_else(|error| panic!("test analysis URL should parse: {error}")),
            time::OffsetDateTime::UNIX_EPOCH,
        );
        pending.push(Action::AnalysisStreamUpdated {
            generation,
            stream_url: analysis_stream.analysis_stream_url(),
        });
        let newest_analysis_stream = ResolvedStream::new(
            media_id,
            url::Url::parse("https://media.invalid/newest-analysis")
                .unwrap_or_else(|error| panic!("test analysis URL should parse: {error}")),
            time::OffsetDateTime::UNIX_EPOCH,
        );
        pending.push(Action::AnalysisStreamUpdated {
            generation,
            stream_url: newest_analysis_stream.analysis_stream_url(),
        });
        pending.push(Action::PlayerEnded { generation });
        pending.push(Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        });
        pending.push(Action::ResolveSucceeded { generation });

        assert!(matches!(
            pending.pop(),
            Some(Action::ResolveSucceeded { .. })
        ));
        assert!(matches!(
            pending.pop(),
            Some(Action::PlayerStatusChanged { .. })
        ));
        assert!(matches!(pending.pop(), Some(Action::PlayerEnded { .. })));
        assert!(matches!(
            pending.pop(),
            Some(Action::ResolvedFormatUpdated { .. })
        ));
        assert!(matches!(
            pending.pop(),
            Some(Action::PreviewStreamUpdated { .. })
        ));
        assert!(matches!(
            pending.pop(),
            Some(Action::AnalysisStreamUpdated {
                stream_url: Some(url),
                ..
            }) if url.as_url().as_str() == "https://media.invalid/newest-analysis"
        ));
        assert!(matches!(
            pending.pop(),
            Some(Action::PlaybackTelemetryUpdated { .. })
        ));
        assert!(matches!(pending.pop(), Some(Action::PlayerProgress { .. })));
        assert!(pending.is_empty());
    }
}
