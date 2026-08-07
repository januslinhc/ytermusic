use std::{
    collections::VecDeque,
    error::Error,
    ffi::{OsStr, OsString},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use futures::FutureExt as _;
use time::macros::datetime;
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use url::Url;
use ytermusic::{
    app::{Action, FadeActivity, Generation},
    config::PlaybackConfig,
    domain::{MediaId, MediaItem, MediaKind},
    player::{
        backend::{
            LoadEpoch, PlayerBackend, PlayerEndReason, PlayerError, PlayerErrorCategory,
            PlayerEvent,
        },
        mpv::MpvBackend,
        supervisor::{PlayerSupervisor, TickSource},
    },
    resolver::{
        AnalysisStreamUrl, CookieFile, PreviewStreamUrl, ResolveError, ResolvePolicy,
        ResolvedStream, Resolver,
    },
};

type TestResult = Result<(), Box<dyn Error>>;

fn missing(message: &'static str) -> std::io::Error {
    std::io::Error::other(message)
}

fn media(video_id: &str, kind: MediaKind) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind,
        title: format!("Title {video_id}"),
        creators: vec!["Creator".to_owned()],
        collection: None,
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}

fn stream(item: &MediaItem, url: &str) -> ResolvedStream {
    let url = match Url::parse(url) {
        Ok(url) => url,
        Err(error) => panic!("test URL should parse: {error}"),
    };
    let mut stream = ResolvedStream::new(item.id.clone(), url, datetime!(2026-07-24 00:00 UTC));
    stream.title = Some(item.title.clone());
    stream.duration_ms = item.duration_ms;
    stream.codec = Some("opus".to_owned());
    stream.format_id = Some("251".to_owned());
    stream
}

fn stream_from_raw(item: &MediaItem, raw_url: &str) -> ResolvedStream {
    let mut stream = ResolvedStream::from_raw_audio_url(
        item.id.clone(),
        raw_url,
        datetime!(2026-07-24 00:00 UTC),
    )
    .unwrap_or_else(|error| panic!("test raw audio URL should parse: {error}"));
    stream.title = Some(item.title.clone());
    stream.duration_ms = item.duration_ms;
    stream.codec = Some("opus".to_owned());
    stream.format_id = Some("251".to_owned());
    stream
}

fn stream_with_preview(item: &MediaItem, audio_url: &str, preview_url: &str) -> ResolvedStream {
    let mut stream = stream(item, audio_url);
    stream.preview_url = Some(
        PreviewStreamUrl::parse(preview_url)
            .unwrap_or_else(|error| panic!("test preview URL should parse: {error}")),
    );
    stream
}

enum ResolvePlan {
    Immediate(&'static str),
    ImmediateWithPreview {
        audio_url: &'static str,
        preview_url: &'static str,
    },
    ImmediateRaw(String),
    AfterCancellation(&'static str),
    AfterSignal(&'static str, Arc<Notify>),
}

struct ResolveCall {
    policy: ResolvePolicy,
    cancel: CancellationToken,
}

struct FakeResolver {
    plans: Mutex<VecDeque<ResolvePlan>>,
    calls: mpsc::UnboundedSender<ResolveCall>,
}

impl FakeResolver {
    fn new(
        plans: impl IntoIterator<Item = ResolvePlan>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<ResolveCall>) {
        let (calls, received_calls) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                plans: Mutex::new(plans.into_iter().collect()),
                calls,
            }),
            received_calls,
        )
    }
}

#[async_trait]
impl Resolver for FakeResolver {
    async fn resolve_with_policy(
        &self,
        item: &MediaItem,
        _auth: Option<&CookieFile>,
        policy: ResolvePolicy,
        cancel: CancellationToken,
    ) -> Result<ResolvedStream, ResolveError> {
        let _ = self.calls.send(ResolveCall {
            policy,
            cancel: cancel.clone(),
        });
        let plan = self
            .plans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| panic!("fake resolver plan exhausted"));
        match plan {
            ResolvePlan::Immediate(url) => Ok(stream(item, url)),
            ResolvePlan::ImmediateWithPreview {
                audio_url,
                preview_url,
            } => Ok(stream_with_preview(item, audio_url, preview_url)),
            ResolvePlan::ImmediateRaw(raw_url) => Ok(stream_from_raw(item, &raw_url)),
            ResolvePlan::AfterCancellation(url) => {
                cancel.cancelled().await;
                Ok(stream(item, url))
            }
            ResolvePlan::AfterSignal(url, signal) => {
                signal.notified().await;
                Ok(stream(item, url))
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum BackendCall {
    Load { url: String, start_ms: Option<u64> },
    Paused(bool),
    Seek(i64),
    Volume(f64),
    Speed(f64),
    Reset,
    Shutdown,
}

struct FakeBackend {
    calls: mpsc::UnboundedSender<BackendCall>,
    events: mpsc::UnboundedReceiver<Result<PlayerEvent, PlayerError>>,
    events_blocked: watch::Receiver<bool>,
    load_results: VecDeque<Result<(), PlayerError>>,
    pause_results: VecDeque<Result<(), PlayerError>>,
    closed_forever: Arc<AtomicBool>,
    event_polls: Arc<AtomicUsize>,
}

struct BackendHarness {
    calls: mpsc::UnboundedReceiver<BackendCall>,
    events: mpsc::UnboundedSender<Result<PlayerEvent, PlayerError>>,
    events_blocked: watch::Sender<bool>,
    closed_forever: Arc<AtomicBool>,
    event_polls: Arc<AtomicUsize>,
}

fn fake_backend() -> (Box<dyn PlayerBackend>, BackendHarness) {
    fake_backend_with_options([], [], false)
}

fn fake_backend_with_load_results(
    load_results: impl IntoIterator<Item = Result<(), PlayerError>>,
) -> (Box<dyn PlayerBackend>, BackendHarness) {
    fake_backend_with_options(load_results, [], false)
}

fn fake_backend_with_options(
    load_results: impl IntoIterator<Item = Result<(), PlayerError>>,
    pause_results: impl IntoIterator<Item = Result<(), PlayerError>>,
    block_events: bool,
) -> (Box<dyn PlayerBackend>, BackendHarness) {
    let (calls, received_calls) = mpsc::unbounded_channel();
    let (events, received_events) = mpsc::unbounded_channel();
    let (events_blocked, blocked_events) = watch::channel(block_events);
    let closed_forever = Arc::new(AtomicBool::new(false));
    let event_polls = Arc::new(AtomicUsize::new(0));
    (
        Box::new(FakeBackend {
            calls,
            events: received_events,
            events_blocked: blocked_events,
            load_results: load_results.into_iter().collect(),
            pause_results: pause_results.into_iter().collect(),
            closed_forever: closed_forever.clone(),
            event_polls: event_polls.clone(),
        }),
        BackendHarness {
            calls: received_calls,
            events,
            events_blocked,
            closed_forever,
            event_polls,
        },
    )
}

#[async_trait]
impl PlayerBackend for FakeBackend {
    async fn load(&mut self, url: &Url, start_ms: Option<u64>) -> Result<(), PlayerError> {
        let _ = self.calls.send(BackendCall::Load {
            url: url.as_str().to_owned(),
            start_ms,
        });
        self.load_results.pop_front().unwrap_or(Ok(()))
    }

    async fn reset_session(&mut self) {
        while self.events.try_recv().is_ok() {}
        let _ = self.calls.send(BackendCall::Reset);
    }

    async fn set_paused(&mut self, paused: bool) -> Result<(), PlayerError> {
        let _ = self.calls.send(BackendCall::Paused(paused));
        self.pause_results.pop_front().unwrap_or(Ok(()))
    }

    async fn seek_relative(&mut self, seconds: i64) -> Result<(), PlayerError> {
        let _ = self.calls.send(BackendCall::Seek(seconds));
        Ok(())
    }

    async fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        let _ = self.calls.send(BackendCall::Volume(volume));
        Ok(())
    }

    async fn set_speed(&mut self, speed: f64) -> Result<(), PlayerError> {
        let _ = self.calls.send(BackendCall::Speed(speed));
        Ok(())
    }

    async fn next_event(&mut self) -> Result<PlayerEvent, PlayerError> {
        self.event_polls.fetch_add(1, Ordering::SeqCst);
        while *self.events_blocked.borrow() {
            if self.events_blocked.changed().await.is_err() {
                break;
            }
        }
        if self.closed_forever.load(Ordering::SeqCst) {
            return Err(PlayerError::new(
                PlayerErrorCategory::Closed,
                "backend is closed",
            ));
        }
        self.events
            .recv()
            .await
            .unwrap_or_else(|| Err(PlayerError::new(PlayerErrorCategory::Closed, "closed")))
    }

    async fn shutdown(&mut self) -> Result<(), PlayerError> {
        let _ = self.calls.send(BackendCall::Shutdown);
        Ok(())
    }
}

struct ManualTicks {
    ticks: mpsc::UnboundedReceiver<Duration>,
}

#[async_trait]
impl TickSource for ManualTicks {
    async fn next_tick(&mut self) -> Option<Duration> {
        self.ticks.recv().await
    }
}

fn playback_config() -> PlaybackConfig {
    PlaybackConfig {
        volume: 80,
        fade_in_ms: 100,
        fade_out_ms: 100,
    }
}

struct Rig {
    player: PlayerSupervisor,
    backend: BackendHarness,
    resolve_calls: mpsc::UnboundedReceiver<ResolveCall>,
    ticks: mpsc::UnboundedSender<Duration>,
}

fn rig(plans: impl IntoIterator<Item = ResolvePlan>) -> Rig {
    rig_with_config(plans, playback_config())
}

fn rig_with_config(plans: impl IntoIterator<Item = ResolvePlan>, config: PlaybackConfig) -> Rig {
    let (resolver, resolve_calls) = FakeResolver::new(plans);
    let (backend, backend_harness) = fake_backend();
    rig_with_parts(resolver, resolve_calls, backend, backend_harness, config)
}

fn rig_with_load_results(
    plans: impl IntoIterator<Item = ResolvePlan>,
    config: PlaybackConfig,
    load_results: impl IntoIterator<Item = Result<(), PlayerError>>,
) -> Rig {
    let (resolver, resolve_calls) = FakeResolver::new(plans);
    let (backend, backend_harness) = fake_backend_with_load_results(load_results);
    rig_with_parts(resolver, resolve_calls, backend, backend_harness, config)
}

#[tokio::test]
async fn supervisor_splits_command_ownership_from_action_stream() -> TestResult {
    let Rig {
        player,
        mut backend,
        ..
    } = rig([]);
    let (controller, mut actions) = player.into_parts();

    controller.shutdown().await?;
    assert_eq!(
        backend
            .calls
            .recv()
            .await
            .ok_or_else(|| missing("backend call stream closed"))?,
        BackendCall::Shutdown
    );
    assert!(actions.next_action().await.is_none());
    Ok(())
}

fn rig_with_blocked_events(
    plans: impl IntoIterator<Item = ResolvePlan>,
    config: PlaybackConfig,
) -> Rig {
    let (resolver, resolve_calls) = FakeResolver::new(plans);
    let (backend, backend_harness) = fake_backend_with_options([], [], true);
    rig_with_parts(resolver, resolve_calls, backend, backend_harness, config)
}

fn rig_with_pause_results(
    plans: impl IntoIterator<Item = ResolvePlan>,
    config: PlaybackConfig,
    pause_results: impl IntoIterator<Item = Result<(), PlayerError>>,
) -> Rig {
    let (resolver, resolve_calls) = FakeResolver::new(plans);
    let (backend, backend_harness) = fake_backend_with_options([], pause_results, false);
    rig_with_parts(resolver, resolve_calls, backend, backend_harness, config)
}

fn rig_with_parts(
    resolver: Arc<FakeResolver>,
    resolve_calls: mpsc::UnboundedReceiver<ResolveCall>,
    backend: Box<dyn PlayerBackend>,
    backend_harness: BackendHarness,
    config: PlaybackConfig,
) -> Rig {
    let (ticks, received_ticks) = mpsc::unbounded_channel();
    let player = PlayerSupervisor::spawn(
        resolver,
        backend,
        config,
        Box::new(ManualTicks {
            ticks: received_ticks,
        }),
    );
    Rig {
        player,
        backend: backend_harness,
        resolve_calls,
        ticks,
    }
}

#[tokio::test]
async fn zero_fades_load_and_replace_without_waiting_for_a_tick() -> TestResult {
    let mut rig = rig_with_config(
        [
            ResolvePlan::Immediate("https://media.invalid/one"),
            ResolvePlan::Immediate("https://media.invalid/two"),
        ],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
    );
    rig.player
        .play(Generation::new(1), media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    let _ = backend_call(&mut rig.backend).await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );

    rig.player
        .play(Generation::new(2), media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/two".to_owned(),
            start_ms: None,
        }
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    rig.player.shutdown().await?;
    Ok(())
}

async fn backend_call(harness: &mut BackendHarness) -> Result<BackendCall, std::io::Error> {
    harness
        .calls
        .recv()
        .await
        .ok_or_else(|| missing("backend call channel closed"))
}

async fn action(player: &mut PlayerSupervisor) -> Result<Action, std::io::Error> {
    loop {
        let action = raw_action(player).await?;
        if !matches!(
            action,
            Action::ResolvedFormatUpdated { .. }
                | Action::PreviewStreamUpdated {
                    preview_url: None,
                    ..
                }
                | Action::AnalysisStreamUpdated { .. }
                | Action::PlaybackTelemetryUpdated { .. }
        ) {
            return Ok(action);
        }
    }
}

async fn raw_action(player: &mut PlayerSupervisor) -> Result<Action, std::io::Error> {
    player
        .next_action()
        .await
        .ok_or_else(|| missing("player action channel closed"))
}

async fn yielded_backend_call(harness: &mut BackendHarness) -> Option<BackendCall> {
    for _ in 0..64 {
        if let Ok(call) = harness.calls.try_recv() {
            return Some(call);
        }
        tokio::task::yield_now().await;
    }
    None
}

async fn yielded_action(player: &mut PlayerSupervisor) -> Option<Action> {
    for _ in 0..64 {
        if let Some(action) = player.next_action().now_or_never().flatten() {
            if matches!(
                action,
                Action::ResolvedFormatUpdated { .. }
                    | Action::PreviewStreamUpdated {
                        preview_url: None,
                        ..
                    }
                    | Action::AnalysisStreamUpdated { .. }
                    | Action::PlaybackTelemetryUpdated { .. }
            ) {
                continue;
            }
            return Some(action);
        }
        tokio::task::yield_now().await;
    }
    None
}

async fn yielded_resolve_call(
    calls: &mut mpsc::UnboundedReceiver<ResolveCall>,
) -> Option<ResolveCall> {
    for _ in 0..64 {
        if let Ok(call) = calls.try_recv() {
            return Some(call);
        }
        tokio::task::yield_now().await;
    }
    None
}

async fn assert_backend_polling_stops(harness: &BackendHarness) {
    let polls = harness.event_polls.load(Ordering::SeqCst);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(harness.event_polls.load(Ordering::SeqCst), polls);
}

async fn file_loaded(
    rig: &mut Rig,
    generation: Generation,
    epoch: LoadEpoch,
    status: ytermusic::domain::PlaybackStatus,
) -> TestResult {
    rig.backend
        .events
        .send(Ok(PlayerEvent::LoadStarted { epoch }))?;
    rig.backend
        .events
        .send(Ok(PlayerEvent::FileLoaded { epoch }))?;
    assert_eq!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged { generation, status }
    );
    Ok(())
}

#[tokio::test]
async fn backend_seam_is_object_usable_and_errors_redact_urls() -> TestResult {
    let (mut backend, _harness) = fake_backend();
    backend
        .load(&Url::parse("https://media.invalid/audio")?, Some(1_250))
        .await?;
    backend.reset_session().await;

    let error = PlayerError::new(
        PlayerErrorCategory::Backend,
        "rejected https://media.invalid/audio?signature=do-not-log",
    );
    assert!(!format!("{error}").contains("do-not-log"));
    assert!(!format!("{error:?}").contains("do-not-log"));
    Ok(())
}

#[test]
fn mpv_process_is_idle_audio_only_and_ignores_user_configuration() {
    assert_eq!(
        MpvBackend::command_arguments(OsStr::new("/private/local/mpv.sock")),
        vec![
            OsString::from("--idle=yes"),
            OsString::from("--no-video"),
            OsString::from("--terminal=no"),
            OsString::from("--no-config"),
            OsString::from("--input-ipc-server=/private/local/mpv.sock"),
        ]
    );
}

#[tokio::test]
async fn play_resolves_loads_at_silence_and_ramps_to_target() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate(
        "https://media.invalid/one?token=private",
    )]);
    let item = media("one", MediaKind::Song);
    let generation = Generation::new(7);

    rig.player.play(generation, item, Some(1_250)).await?;
    let call = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing resolve call"))?;
    assert_eq!(call.policy, ResolvePolicy::UseCache);
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/one?token=private".to_owned(),
            start_ms: Some(1_250),
        }
    );
    assert_eq!(
        action(&mut rig.player).await?,
        Action::ResolveSucceeded { generation }
    );

    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(40.0)
    );
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn accepted_resolution_emits_safe_quality_then_fade_telemetry() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate(
        "https://media.invalid/audio?signature=never-log-this",
    )]);
    let generation = Generation::new(70);
    rig.player
        .play(generation, media("quality", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;

    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::ResolveSucceeded { generation }
    );
    let quality =
        tokio::time::timeout(Duration::from_millis(250), raw_action(&mut rig.player)).await??;
    assert_eq!(
        quality,
        Action::ResolvedFormatUpdated {
            generation,
            quality: ytermusic::app::ResolverQuality::new(Some("opus"), Some("251")),
        }
    );
    let debug = format!("{quality:?}");
    assert!(!debug.contains("never-log-this"));
    assert!(!debug.contains("https://"));
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PreviewStreamUpdated {
            generation,
            preview_url: None,
        }
    );
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::AnalysisStreamUpdated {
            generation: action_generation,
            stream_url: Some(_),
        } if action_generation == generation
    ));
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 0.0,
            fade: Some(FadeActivity::In),
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn preview_is_emitted_separately_and_backend_receives_only_audio_url() -> TestResult {
    let mut rig = rig([ResolvePlan::ImmediateWithPreview {
        audio_url: "https://media.invalid/audio?signature=audio-secret",
        preview_url: "https://video.invalid/preview?signature=preview-secret",
    }]);
    let generation = Generation::new(75);
    rig.player
        .play(generation, media("preview", MediaKind::Video), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/audio?signature=audio-secret".to_owned(),
            start_ms: None,
        }
    );

    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::ResolveSucceeded { generation }
    );
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::ResolvedFormatUpdated {
            generation: action_generation,
            ..
        } if action_generation == generation
    ));
    let preview_action = raw_action(&mut rig.player).await?;
    assert!(matches!(
        &preview_action,
        Action::PreviewStreamUpdated {
            generation: action_generation,
            preview_url: Some(url),
        } if *action_generation == generation
            && url.as_url().as_str()
                == "https://video.invalid/preview?signature=preview-secret"
    ));
    let debug = format!("{preview_action:?}");
    assert!(!debug.contains("video.invalid"));
    assert!(!debug.contains("preview-secret"));
    assert!(yielded_backend_call(&mut rig.backend).await.is_none());

    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::AnalysisStreamUpdated {
            generation: action_generation,
            stream_url: Some(_),
        } if action_generation == generation
    ));

    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 0.0,
            fade: Some(FadeActivity::In),
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn analysis_stream_is_emitted_separately_without_an_extra_resolve_or_backend_load()
-> TestResult {
    let audio_url = "https://media.invalid/audio?signature=analysis-secret";
    let mut rig = rig([ResolvePlan::Immediate(audio_url)]);
    let generation = Generation::new(175);
    rig.player
        .play(generation, media("analysis", MediaKind::Song), None)
        .await?;
    let resolve_call = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing resolve"))?;
    assert_eq!(resolve_call.policy, ResolvePolicy::UseCache);
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: audio_url.to_owned(),
            start_ms: None,
        }
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::ResolveSucceeded { generation }
    );
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::ResolvedFormatUpdated { .. }
    ));
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::PreviewStreamUpdated { .. }
    ));
    let analysis_action = raw_action(&mut rig.player).await?;
    assert!(matches!(
        &analysis_action,
        Action::AnalysisStreamUpdated {
            generation: action_generation,
            stream_url: Some(url),
        } if *action_generation == generation && url.as_url().as_str() == audio_url
    ));
    let debug = format!("{analysis_action:?}");
    assert!(!debug.contains("media.invalid"));
    assert!(!debug.contains("analysis-secret"));
    assert!(yielded_backend_call(&mut rig.backend).await.is_none());
    assert!(yielded_resolve_call(&mut rig.resolve_calls).await.is_none());

    let _type_check: Option<&AnalysisStreamUrl> = match &analysis_action {
        Action::AnalysisStreamUpdated { stream_url, .. } => stream_url.as_ref(),
        _ => None,
    };
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn oversized_raw_analysis_url_still_loads_audio_once_and_emits_none() -> TestResult {
    let raw_url = format!(
        "https://media.invalid/{}audio?token=raw-supervisor-secret",
        "segment/../".repeat(800)
    );
    let canonical_url = Url::parse(&raw_url)?.to_string();
    assert!(raw_url.len() > 8_192);
    assert!(canonical_url.len() < 8_192);
    let mut rig = rig([ResolvePlan::ImmediateRaw(raw_url)]);
    let generation = Generation::new(176);
    rig.player
        .play(generation, media("raw-analysis", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: canonical_url,
            start_ms: None,
        }
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::ResolveSucceeded { generation }
    );
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::ResolvedFormatUpdated { .. }
    ));
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::PreviewStreamUpdated { .. }
    ));
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::AnalysisStreamUpdated {
            generation,
            stream_url: None,
        }
    );
    assert!(yielded_backend_call(&mut rig.backend).await.is_none());
    assert!(yielded_resolve_call(&mut rig.resolve_calls).await.is_none());
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn same_generation_refresh_clears_preview_and_rejected_analysis_stream() -> TestResult {
    let mut rig = rig([
        ResolvePlan::ImmediateWithPreview {
            audio_url: "https://media.invalid/stale-audio",
            preview_url: "https://video.invalid/stale-preview",
        },
        ResolvePlan::Immediate("http://media.invalid/fresh-audio"),
    ]);
    let generation = Generation::new(76);
    rig.player
        .play(generation, media("refresh-preview", MediaKind::Video), None)
        .await?;
    let initial = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing initial resolve"))?;
    assert_eq!(initial.policy, ResolvePolicy::UseCache);
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::ResolveSucceeded { generation }
    );
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::ResolvedFormatUpdated { .. }
    ));
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::PreviewStreamUpdated {
            generation: action_generation,
            preview_url: Some(_),
        } if action_generation == generation
    ));
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::AnalysisStreamUpdated {
            generation: action_generation,
            stream_url: Some(_),
        } if action_generation == generation
    ));
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated { .. }
    ));
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(1),
        reason: PlayerEndReason::UrlRejected,
    }))?;
    let refresh = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing refresh resolve"))?;
    assert_eq!(refresh.policy, ResolvePolicy::ForceRefresh);
    let _ = backend_call(&mut rig.backend).await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "http://media.invalid/fresh-audio".to_owned(),
            start_ms: None,
        }
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::ResolveSucceeded { generation }
    );
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::ResolvedFormatUpdated { .. }
    ));
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PreviewStreamUpdated {
            generation,
            preview_url: None,
        }
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::AnalysisStreamUpdated {
            generation,
            stream_url: None,
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn zero_duration_fade_emits_the_exact_endpoint_with_no_activity() -> TestResult {
    let mut rig = rig_with_config(
        [ResolvePlan::Immediate("https://media.invalid/zero")],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
    );
    let generation = Generation::new(72);
    rig.player
        .play(generation, media("zero", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::ResolveSucceeded { generation }
    );
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::ResolvedFormatUpdated {
            generation: quality_generation,
            ..
        } if quality_generation == generation
    ));
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PreviewStreamUpdated {
            generation,
            preview_url: None,
        }
    );
    assert!(matches!(
        raw_action(&mut rig.player).await?,
        Action::AnalysisStreamUpdated {
            generation: action_generation,
            stream_url: Some(_),
        } if action_generation == generation
    ));
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 80.0,
            fade: None,
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn replacement_fade_uses_the_pending_attempt_generation() -> TestResult {
    let replacement_ready = Arc::new(Notify::new());
    let first_generation = Generation::new(73);
    let replacement_generation = Generation::new(74);
    let mut rig = rig([
        ResolvePlan::Immediate("https://media.invalid/first"),
        ResolvePlan::AfterSignal("https://media.invalid/replacement", replacement_ready),
    ]);
    rig.player
        .play(first_generation, media("first", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = raw_action(&mut rig.player).await?;
    let _ = raw_action(&mut rig.player).await?;
    let _ = raw_action(&mut rig.player).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(1),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(1),
    }))?;
    let _ = action(&mut rig.player).await?;
    rig.ticks.send(Duration::from_millis(100))?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = raw_action(&mut rig.player).await?;

    rig.player
        .play(
            replacement_generation,
            media("replacement", MediaKind::Song),
            None,
        )
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation: replacement_generation,
            effective_volume: 80.0,
            fade: Some(FadeActivity::Out),
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn manual_ticks_emit_in_out_and_idle_fade_telemetry() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/telemetry")]);
    let generation = Generation::new(71);
    rig.player
        .play(generation, media("telemetry", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = raw_action(&mut rig.player).await?;
    let _ = raw_action(&mut rig.player).await?;
    let _ = raw_action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(40.0)
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 40.0,
            fade: Some(FadeActivity::In),
        }
    );
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 80.0,
            fade: None,
        }
    );

    rig.player.pause().await?;
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 80.0,
            fade: Some(FadeActivity::Out),
        }
    );
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(40.0)
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 40.0,
            fade: Some(FadeActivity::Out),
        }
    );
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        raw_action(&mut rig.player).await?,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 0.0,
            fade: None,
        }
    );
    let _ = backend_call(&mut rig.backend).await?;
    assert_eq!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation,
            status: ytermusic::domain::PlaybackStatus::Paused,
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn pause_fades_before_pausing_and_resume_unpauses_before_fading() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/one")]);
    let generation = Generation::new(1);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;
    rig.ticks.send(Duration::from_millis(100))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );

    rig.player.pause().await?;
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(40.0)
    );
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Paused(true)
    );

    rig.player.resume().await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Paused(false)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    rig.ticks.send(Duration::from_millis(100))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn pause_during_resolution_loads_new_media_silently_and_keeps_it_paused() -> TestResult {
    let resolved = Arc::new(Notify::new());
    let mut rig = rig([ResolvePlan::AfterSignal(
        "https://media.invalid/one",
        resolved.clone(),
    )]);
    rig.player
        .play(Generation::new(31), media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    rig.player.pause().await?;

    resolved.notify_one();
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/one".to_owned(),
            start_ms: None,
        }
    );
    let _ = action(&mut rig.player).await?;
    assert_eq!(
        yielded_backend_call(&mut rig.backend).await,
        Some(BackendCall::Paused(true))
    );

    rig.ticks.send(Duration::from_millis(100))?;
    assert_eq!(yielded_backend_call(&mut rig.backend).await, None);
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resume_during_pending_replacement_preserves_fade_out_then_fades_new_media_in() -> TestResult
{
    let replacement = Arc::new(Notify::new());
    let first_generation = Generation::new(32);
    let mut rig = rig([
        ResolvePlan::Immediate("https://media.invalid/one"),
        ResolvePlan::AfterSignal("https://media.invalid/two", replacement.clone()),
    ]);
    rig.player
        .play(first_generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        first_generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;
    rig.ticks.send(Duration::from_millis(100))?;
    let _ = backend_call(&mut rig.backend).await?;

    rig.player
        .play(Generation::new(33), media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(40.0)
    );

    rig.player.pause().await?;
    rig.player.resume().await?;
    assert_eq!(yielded_backend_call(&mut rig.backend).await, None);
    assert_eq!(yielded_action(&mut rig.player).await, None);
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );

    replacement.notify_one();
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/two".to_owned(),
            start_ms: None,
        }
    );
    let _ = action(&mut rig.player).await?;
    rig.ticks.send(Duration::from_millis(100))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn resume_interrupts_a_pause_fade_without_repausing_at_fade_completion() -> TestResult {
    let generation = Generation::new(38);
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/one")]);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;
    rig.ticks.send(Duration::from_millis(100))?;
    let _ = backend_call(&mut rig.backend).await?;

    rig.player.pause().await?;
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(40.0)
    );
    rig.player.resume().await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Paused(false)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    rig.ticks.send(Duration::from_millis(100))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    assert_eq!(yielded_backend_call(&mut rig.backend).await, None);
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn pause_and_resume_after_load_reply_control_the_staged_media() -> TestResult {
    let generation = Generation::new(39);
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/one")]);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;

    rig.player.pause().await?;
    assert_eq!(
        yielded_backend_call(&mut rig.backend).await,
        Some(BackendCall::Volume(0.0))
    );
    assert_eq!(
        yielded_backend_call(&mut rig.backend).await,
        Some(BackendCall::Paused(true))
    );
    rig.player.resume().await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Paused(false)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    rig.ticks.send(Duration::from_millis(100))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn replace_cancels_old_resolution_and_waits_for_fade_out_before_load() -> TestResult {
    let mut rig = rig([
        ResolvePlan::AfterCancellation("https://media.invalid/stale"),
        ResolvePlan::Immediate("https://media.invalid/two"),
    ]);
    rig.player
        .play(Generation::new(1), media("one", MediaKind::Song), None)
        .await?;
    let first = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing first resolve"))?;

    rig.player
        .play(Generation::new(2), media("two", MediaKind::Song), None)
        .await?;
    let second = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing second resolve"))?;
    assert!(first.cancel.is_cancelled());
    assert_eq!(second.policy, ResolvePolicy::UseCache);
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/two".to_owned(),
            start_ms: None,
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn replacing_during_fade_reverses_it_before_loading_next_url() -> TestResult {
    let mut rig = rig([
        ResolvePlan::Immediate("https://media.invalid/one"),
        ResolvePlan::Immediate("https://media.invalid/two"),
    ]);
    rig.player
        .play(Generation::new(1), media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(40.0)
    );

    rig.player
        .play(Generation::new(2), media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(20.0)
    );
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/two".to_owned(),
            start_ms: None,
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn queued_outgoing_load_does_not_bind_to_the_newer_staged_attempt() -> TestResult {
    let mut rig = rig_with_blocked_events(
        [
            ResolvePlan::Immediate("https://media.invalid/one"),
            ResolvePlan::Immediate("https://media.invalid/two"),
            ResolvePlan::Immediate("https://media.invalid/unexpected-refresh"),
        ],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
    );
    let first_generation = Generation::new(34);
    let second_generation = Generation::new(35);
    rig.player
        .play(first_generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(1),
    }))?;

    rig.player
        .play(second_generation, media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;

    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(1),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::Progress {
        epoch: LoadEpoch::new(1),
        position_ms: 179_900,
        duration_ms: Some(180_000),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(1),
        reason: PlayerEndReason::Natural,
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(1),
        reason: PlayerEndReason::UrlRejected,
    }))?;
    rig.backend.events_blocked.send(false)?;
    assert_eq!(yielded_action(&mut rig.player).await, None);
    assert!(rig.resolve_calls.try_recv().is_err());

    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(2),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(2),
    }))?;
    assert_eq!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation: second_generation,
            status: ytermusic::domain::PlaybackStatus::Playing,
        }
    );
    rig.backend.events.send(Ok(PlayerEvent::Progress {
        epoch: LoadEpoch::new(2),
        position_ms: 500,
        duration_ms: Some(180_000),
    }))?;
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::PlayerProgress {
            generation,
            position_ms: 500,
            ..
        } if generation == second_generation
    ));
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn failed_load_without_start_file_does_not_offset_the_next_submission() -> TestResult {
    let first_generation = Generation::new(37);
    let second_generation = Generation::new(38);
    let mut rig = rig_with_load_results(
        [
            ResolvePlan::Immediate("https://media.invalid/rejected"),
            ResolvePlan::Immediate("https://media.invalid/accepted"),
        ],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
        [
            Err(PlayerError::new(
                PlayerErrorCategory::Command,
                "load rejected",
            )),
            Ok(()),
        ],
    );

    rig.player
        .play(first_generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert!(matches!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load { .. }
    ));
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::ResolveFailed { generation, .. } if generation == first_generation
    ));

    rig.player
        .play(second_generation, media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert!(matches!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load { .. }
    ));
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    assert_eq!(
        action(&mut rig.player).await?,
        Action::ResolveSucceeded {
            generation: second_generation,
        }
    );

    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(1),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(1),
    }))?;
    assert_eq!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation: second_generation,
            status: ytermusic::domain::PlaybackStatus::Playing,
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn failed_replacement_clears_older_tombstones_before_a_later_load() -> TestResult {
    let first_generation = Generation::new(40);
    let failed_generation = Generation::new(41);
    let final_generation = Generation::new(42);
    let mut rig = rig_with_load_results(
        [
            ResolvePlan::Immediate("https://media.invalid/first"),
            ResolvePlan::Immediate("https://media.invalid/rejected"),
            ResolvePlan::Immediate("https://media.invalid/final"),
        ],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
        [
            Ok(()),
            Err(PlayerError::new(
                PlayerErrorCategory::Command,
                "load rejected",
            )),
            Ok(()),
        ],
    );

    rig.player
        .play(first_generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;

    rig.player
        .play(failed_generation, media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::ResolveFailed { generation, .. } if generation == failed_generation
    ));

    rig.player
        .play(final_generation, media("three", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(1),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(1),
    }))?;

    assert_eq!(
        yielded_action(&mut rig.player).await,
        Some(Action::PlayerStatusChanged {
            generation: final_generation,
            status: ytermusic::domain::PlaybackStatus::Playing,
        })
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn paused_load_session_loss_does_not_shift_the_next_sessions_epoch() -> TestResult {
    let first_generation = Generation::new(43);
    let second_generation = Generation::new(44);
    let resolved = Arc::new(Notify::new());
    let mut rig = rig_with_pause_results(
        [
            ResolvePlan::AfterSignal("https://media.invalid/first", resolved.clone()),
            ResolvePlan::Immediate("https://media.invalid/second"),
        ],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
        [Err(PlayerError::new(
            PlayerErrorCategory::Closed,
            "paused-state command lost its session",
        ))],
    );

    rig.player
        .play(first_generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    rig.player.pause().await?;
    resolved.notify_one();
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert!(matches!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load { .. }
    ));
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Paused(true)
    );
    assert_eq!(backend_call(&mut rig.backend).await?, BackendCall::Reset);
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::ResolveFailed { generation, .. } if generation == first_generation
    ));

    rig.player
        .play(second_generation, media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert!(matches!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load { .. }
    ));
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    let _ = action(&mut rig.player).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(1),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(1),
    }))?;

    assert_eq!(
        yielded_action(&mut rig.player).await,
        Some(Action::PlayerStatusChanged {
            generation: second_generation,
            status: ytermusic::domain::PlaybackStatus::Playing,
        })
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn paused_restart_session_loss_is_bounded_and_does_not_shift_a_later_load() -> TestResult {
    let first_generation = Generation::new(45);
    let second_generation = Generation::new(46);
    let mut rig = rig_with_pause_results(
        [
            ResolvePlan::Immediate("https://media.invalid/first"),
            ResolvePlan::Immediate("https://media.invalid/second"),
        ],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
        [
            Ok(()),
            Err(PlayerError::new(
                PlayerErrorCategory::Closed,
                "restart paused-state command lost its session",
            )),
        ],
    );

    rig.player
        .play(first_generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        first_generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;
    rig.player.pause().await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Paused(true)
    );
    let _ = action(&mut rig.player).await?;

    rig.backend.events.send(Ok(PlayerEvent::Shutdown))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert!(matches!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load { .. }
    ));
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Paused(true)
    );
    assert_eq!(backend_call(&mut rig.backend).await?, BackendCall::Reset);
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation,
            status: ytermusic::domain::PlaybackStatus::Failed,
        } if generation == first_generation
    ));
    assert_backend_polling_stops(&rig.backend).await;

    rig.player
        .play(second_generation, media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert!(matches!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load { .. }
    ));
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );
    let _ = action(&mut rig.player).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(2),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(2),
    }))?;

    assert_eq!(
        yielded_action(&mut rig.player).await,
        Some(Action::PlayerStatusChanged {
            generation: second_generation,
            status: ytermusic::domain::PlaybackStatus::Playing,
        })
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn restart_discards_a_submission_that_never_started() -> TestResult {
    let generation = Generation::new(39);
    let mut rig = rig_with_config(
        [ResolvePlan::Immediate("https://media.invalid/one")],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
    );
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;

    rig.backend.events.send(Ok(PlayerEvent::Shutdown))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert!(matches!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load { .. }
    ));
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(80.0)
    );

    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(1),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(1),
    }))?;
    assert_eq!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation,
            status: ytermusic::domain::PlaybackStatus::Playing,
        }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn staged_url_rejection_before_file_loaded_force_refreshes_exactly_once() -> TestResult {
    let generation = Generation::new(36);
    let refreshed = Arc::new(Notify::new());
    let mut rig = rig([
        ResolvePlan::Immediate("https://media.invalid/stale"),
        ResolvePlan::AfterSignal("https://media.invalid/fresh", refreshed.clone()),
    ]);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let initial = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing initial resolve"))?;
    assert_eq!(initial.policy, ResolvePolicy::UseCache);
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(1),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(1),
        reason: PlayerEndReason::UrlRejected,
    }))?;

    let refresh = yielded_resolve_call(&mut rig.resolve_calls)
        .await
        .ok_or_else(|| missing("staged URL rejection did not trigger a refresh"))?;
    assert_eq!(refresh.policy, ResolvePolicy::ForceRefresh);
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(1),
    }))?;
    assert_eq!(yielded_action(&mut rig.player).await, None);
    refreshed.notify_one();
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(2),
    }))?;
    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(2),
        reason: PlayerEndReason::UrlRejected,
    }))?;

    assert!(matches!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation: failed_generation,
            status: ytermusic::domain::PlaybackStatus::Failed,
        } if failed_generation == generation
    ));
    assert!(rig.resolve_calls.try_recv().is_err());
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn staged_replacement_commands_do_not_emit_status_for_the_outgoing_generation() -> TestResult
{
    let mut rig = rig_with_config(
        [
            ResolvePlan::Immediate("https://media.invalid/one"),
            ResolvePlan::Immediate("https://media.invalid/two"),
        ],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
    );
    let first_generation = Generation::new(41);
    let second_generation = Generation::new(42);
    rig.player
        .play(first_generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        first_generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    rig.player
        .play(second_generation, media("two", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;

    rig.player.pause().await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Paused(true)
    );
    assert_eq!(yielded_action(&mut rig.player).await, None);
    file_loaded(
        &mut rig,
        second_generation,
        LoadEpoch::new(2),
        ytermusic::domain::PlaybackStatus::Paused,
    )
    .await?;
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn natural_end_emits_the_current_attempt_generation() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/one")]);
    let generation = Generation::new(42);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;
    rig.ticks.send(Duration::from_millis(100))?;
    let _ = backend_call(&mut rig.backend).await?;

    rig.backend.events.send(Ok(PlayerEvent::Progress {
        epoch: LoadEpoch::new(1),
        position_ms: 99_950,
        duration_ms: Some(100_000),
    }))?;
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::PlayerProgress {
            generation: progress_generation,
            position_ms: 99_950,
            duration_ms: Some(100_000),
            ..
        } if progress_generation == generation
    ));
    rig.ticks.send(Duration::from_millis(50))?;
    assert_eq!(
        yielded_backend_call(&mut rig.backend).await,
        Some(BackendCall::Volume(40.0))
    );

    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(1),
        reason: PlayerEndReason::Natural,
    }))?;
    assert_eq!(
        action(&mut rig.player).await?,
        Action::PlayerEnded { generation }
    );
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn rejected_url_gets_exactly_one_force_refresh() -> TestResult {
    let mut rig = rig([
        ResolvePlan::Immediate("https://media.invalid/stale"),
        ResolvePlan::Immediate("https://media.invalid/fresh"),
    ]);
    let generation = Generation::new(3);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let first = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing initial resolve"))?;
    assert_eq!(first.policy, ResolvePolicy::UseCache);
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(1),
        reason: PlayerEndReason::UrlRejected,
    }))?;
    let refresh = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing refresh resolve"))?;
    assert_eq!(refresh.policy, ResolvePolicy::ForceRefresh);
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(2),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(2),
        reason: PlayerEndReason::UrlRejected,
    }))?;
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation: failed_generation,
            status: ytermusic::domain::PlaybackStatus::Failed,
        } if failed_generation == generation
    ));
    assert!(rig.resolve_calls.try_recv().is_err());
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn backend_shutdown_reloads_at_most_once_for_the_current_attempt() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/one")]);
    let generation = Generation::new(9);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    rig.backend.events.send(Ok(PlayerEvent::Shutdown))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/one".to_owned(),
            start_ms: None,
        }
    );
    rig.backend.events.send(Ok(PlayerEvent::Shutdown))?;
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation: failed_generation,
            status: ytermusic::domain::PlaybackStatus::Failed,
        } if failed_generation == generation
    ));
    assert!(rig.backend.calls.try_recv().is_err());
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn backend_shutdown_before_file_loaded_restarts_the_staged_attempt() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/one")]);
    let generation = Generation::new(40);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(1),
    }))?;

    rig.backend.events.send(Ok(PlayerEvent::Shutdown))?;
    assert_eq!(
        yielded_backend_call(&mut rig.backend).await,
        Some(BackendCall::Volume(0.0))
    );
    assert_eq!(
        yielded_backend_call(&mut rig.backend).await,
        Some(BackendCall::Load {
            url: "https://media.invalid/one".to_owned(),
            start_ms: None,
        })
    );
    rig.backend.events.send(Ok(PlayerEvent::FileLoaded {
        epoch: LoadEpoch::new(1),
    }))?;
    assert_eq!(yielded_action(&mut rig.player).await, None);
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(2),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn refresh_does_not_reset_the_restart_budget_for_the_same_generation() -> TestResult {
    let mut rig = rig([
        ResolvePlan::Immediate("https://media.invalid/stale"),
        ResolvePlan::Immediate("https://media.invalid/fresh"),
    ]);
    let generation = Generation::new(19);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    rig.backend.events.send(Ok(PlayerEvent::Shutdown))?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Volume(0.0)
    );
    let _ = backend_call(&mut rig.backend).await?;
    rig.backend.events.send(Ok(PlayerEvent::LoadStarted {
        epoch: LoadEpoch::new(2),
    }))?;

    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(2),
        reason: PlayerEndReason::UrlRejected,
    }))?;
    let refresh = rig
        .resolve_calls
        .recv()
        .await
        .ok_or_else(|| missing("missing refresh after restart"))?;
    assert_eq!(refresh.policy, ResolvePolicy::ForceRefresh);
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;

    rig.backend.events.send(Ok(PlayerEvent::Shutdown))?;
    assert!(matches!(
        yielded_action(&mut rig.player).await,
        Some(Action::PlayerStatusChanged {
            generation: failed_generation,
            status: ytermusic::domain::PlaybackStatus::Failed,
        }) if failed_generation == generation
    ));
    assert!(rig.backend.calls.try_recv().is_err());
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_backend_failure_stops_polling_and_emits_failure_once() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/one")]);
    let generation = Generation::new(23);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    rig.backend.closed_forever.store(true, Ordering::SeqCst);
    rig.backend.events.send(Err(PlayerError::new(
        PlayerErrorCategory::Closed,
        "backend is closed",
    )))?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    assert!(matches!(
        action(&mut rig.player).await?,
        Action::PlayerStatusChanged {
            generation: failed_generation,
            status: ytermusic::domain::PlaybackStatus::Failed,
        } if failed_generation == generation
    ));

    let polls_after_failure = rig.backend.event_polls.load(Ordering::SeqCst);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        rig.backend.event_polls.load(Ordering::SeqCst),
        polls_after_failure
    );
    assert!(rig.player.next_action().now_or_never().is_none());
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_preempts_a_saturated_player_action_channel() -> TestResult {
    let mut rig = rig_with_config(
        [ResolvePlan::Immediate("https://media.invalid/one")],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
    );
    let generation = Generation::new(36);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    for position_ms in 0..96 {
        rig.backend.events.send(Ok(PlayerEvent::Progress {
            epoch: LoadEpoch::new(1),
            position_ms,
            duration_ms: None,
        }))?;
    }
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }

    let shutdown = tokio::spawn(rig.player.shutdown());
    for _ in 0..256 {
        if shutdown.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), backend_call(&mut rig.backend)).await??,
        BackendCall::Shutdown
    );
    tokio::time::timeout(Duration::from_secs(1), shutdown).await???;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_progress_coalescing_retains_the_terminal_action() -> TestResult {
    let mut rig = rig_with_config(
        [ResolvePlan::Immediate("https://media.invalid/one")],
        PlaybackConfig {
            volume: 80,
            fade_in_ms: 0,
            fade_out_ms: 0,
        },
    );
    let generation = Generation::new(37);
    rig.player
        .play(generation, media("one", MediaKind::Song), None)
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = backend_call(&mut rig.backend).await?;
    let _ = action(&mut rig.player).await?;
    file_loaded(
        &mut rig,
        generation,
        LoadEpoch::new(1),
        ytermusic::domain::PlaybackStatus::Playing,
    )
    .await?;

    let polls_before_burst = rig.backend.event_polls.load(Ordering::SeqCst);
    for position_ms in 0..96 {
        rig.backend.events.send(Ok(PlayerEvent::Progress {
            epoch: LoadEpoch::new(1),
            position_ms,
            duration_ms: None,
        }))?;
    }
    rig.backend.events.send(Ok(PlayerEvent::Ended {
        epoch: LoadEpoch::new(1),
        reason: PlayerEndReason::Natural,
    }))?;
    tokio::time::timeout(Duration::from_secs(1), async {
        while rig.backend.event_polls.load(Ordering::SeqCst) < polls_before_burst + 97 {
            tokio::task::yield_now().await;
        }
    })
    .await?;

    let ended_generation = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Action::PlayerEnded { generation } = raw_action(&mut rig.player).await? {
                return Ok::<_, std::io::Error>(generation);
            }
        }
    })
    .await??;
    assert_eq!(ended_generation, generation);
    rig.player.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn podcast_seek_and_speed_are_forwarded_without_wall_clock_waits() -> TestResult {
    let mut rig = rig([ResolvePlan::Immediate("https://media.invalid/podcast")]);
    rig.player
        .play(
            Generation::new(1),
            media("episode", MediaKind::PodcastEpisode),
            Some(30_000),
        )
        .await?;
    let _ = rig.resolve_calls.recv().await;
    let _ = backend_call(&mut rig.backend).await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Load {
            url: "https://media.invalid/podcast".to_owned(),
            start_ms: Some(30_000),
        }
    );

    rig.player.seek_relative(-15).await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Seek(-15)
    );
    rig.player.set_speed(1.75).await?;
    assert_eq!(
        backend_call(&mut rig.backend).await?,
        BackendCall::Speed(1.75)
    );
    rig.player.shutdown().await?;
    Ok(())
}
