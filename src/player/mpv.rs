use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    fmt,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use serde_json::{Number, Value};
use tokio::{process::Child, time::Instant};
use url::Url;

use crate::platform::{IpcEndpoint, MpvConnector, NativeMpvConnector};

use super::{
    backend::{
        LoadEpoch, PlayerBackend, PlayerEndReason, PlayerError, PlayerErrorCategory, PlayerEvent,
    },
    protocol::{MpvEvent, MpvMessage, MpvReply, MpvRequest, RequestIdAllocator},
    transport::MpvTransport,
};

const MAX_MPV_LINE_BYTES: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_RETRY: Duration = Duration::from_millis(20);
const COMMAND_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

const OBSERVE_TIME_POS: u64 = 1;
const OBSERVE_DURATION: u64 = 2;
const OBSERVE_PAUSE: u64 = 3;
const OBSERVE_VOLUME: u64 = 4;
const OBSERVE_SPEED: u64 = 5;

pub struct MpvBackend {
    executable: PathBuf,
    connector: Arc<dyn MpvConnector>,
    reply_timeout: Duration,
    next_load_epoch: Option<u64>,
    session: Option<Session>,
}

struct Session {
    endpoint: IpcEndpoint,
    child: Child,
    transport: MpvTransport,
    request_ids: RequestIdAllocator,
    pending_events: VecDeque<MpvEvent>,
    pending_loads: VecDeque<PendingLoad>,
    active_load: Option<ActiveLoad>,
    pending_file_loaded: Option<PendingFileLoaded>,
    position_ms: u64,
    duration_ms: Option<u64>,
}

struct PendingLoad {
    start_ms: Option<u64>,
}

struct ActiveLoad {
    epoch: LoadEpoch,
    start_ms: Option<u64>,
    file_loaded_delivered: bool,
}

struct PendingFileLoaded {
    epoch: LoadEpoch,
    request: MpvRequest,
    phase: PostLoadSeekPhase,
}

#[derive(Clone, Copy)]
enum PostLoadSeekPhase {
    ReadyToSend,
    Sending,
    AwaitingReply { request_id: u64, deadline: Instant },
    ReplyReceived { success: bool },
    ReplyTimedOut,
}

impl MpvBackend {
    /// Starts a private, configuration-isolated mpv process.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe spawn, connection, or protocol error.
    pub async fn spawn(executable: impl Into<PathBuf>) -> Result<Self, PlayerError> {
        Self::spawn_with_connector(executable, Arc::new(NativeMpvConnector)).await
    }

    /// Starts mpv using an injected local-IPC connector.
    ///
    /// This exists for platform composition and transport-level tests; the
    /// endpoint itself is always created by [`IpcEndpoint::native`].
    ///
    /// # Errors
    ///
    /// Returns a secret-safe spawn, connection, or protocol error.
    pub async fn spawn_with_connector(
        executable: impl Into<PathBuf>,
        connector: Arc<dyn MpvConnector>,
    ) -> Result<Self, PlayerError> {
        Self::spawn_with_connector_and_timeout(executable, connector, COMMAND_REPLY_TIMEOUT).await
    }

    /// Starts mpv with an injected connector and command-reply timeout.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe spawn, connection, timeout, or protocol error.
    pub async fn spawn_with_connector_and_timeout(
        executable: impl Into<PathBuf>,
        connector: Arc<dyn MpvConnector>,
        reply_timeout: Duration,
    ) -> Result<Self, PlayerError> {
        let mut backend = Self {
            executable: executable.into(),
            connector,
            reply_timeout,
            next_load_epoch: Some(1),
            session: None,
        };
        backend.start_session().await?;
        Ok(backend)
    }

    #[must_use]
    pub fn command_arguments(endpoint: &OsStr) -> Vec<OsString> {
        let mut ipc_argument = OsString::from("--input-ipc-server=");
        ipc_argument.push(endpoint);
        vec![
            OsString::from("--idle=yes"),
            OsString::from("--no-video"),
            OsString::from("--terminal=no"),
            OsString::from("--no-config"),
            ipc_argument,
        ]
    }

    async fn ensure_session(&mut self) -> Result<(), PlayerError> {
        if self.session.is_none() {
            self.start_session().await?;
        }
        Ok(())
    }

    async fn start_session(&mut self) -> Result<(), PlayerError> {
        let endpoint = IpcEndpoint::native().map_err(|_| {
            PlayerError::new(
                PlayerErrorCategory::Connection,
                "could not create a private mpv IPC endpoint",
            )
        })?;
        let mut command = tokio::process::Command::new(&self.executable);
        command
            .args(Self::command_arguments(endpoint.as_os_str()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|_| {
            PlayerError::new(
                PlayerErrorCategory::Spawn,
                "could not start the mpv playback process",
            )
        })?;

        let deadline = Instant::now() + CONNECT_TIMEOUT;
        let stream = loop {
            if let Ok(stream) = self.connector.connect(&endpoint).await {
                break stream;
            }
            if child
                .try_wait()
                .map_err(|_| backend_wait_error())?
                .is_some()
            {
                return Err(PlayerError::new(
                    PlayerErrorCategory::Spawn,
                    "mpv stopped before its local IPC endpoint became ready",
                ));
            }
            if Instant::now() >= deadline {
                terminate(&mut child).await;
                return Err(PlayerError::new(
                    PlayerErrorCategory::Connection,
                    "mpv local IPC did not become ready in time",
                ));
            }
            tokio::time::sleep(CONNECT_RETRY).await;
        };
        let transport = MpvTransport::new(stream, MAX_MPV_LINE_BYTES).map_err(|_| {
            PlayerError::new(
                PlayerErrorCategory::Protocol,
                "could not initialize the mpv protocol transport",
            )
        })?;
        self.session = Some(Session {
            endpoint,
            child,
            transport,
            request_ids: RequestIdAllocator::default(),
            pending_events: VecDeque::new(),
            pending_loads: VecDeque::new(),
            active_load: None,
            pending_file_loaded: None,
            position_ms: 0,
            duration_ms: None,
        });

        for (observer_id, property) in [
            (OBSERVE_TIME_POS, "time-pos"),
            (OBSERVE_DURATION, "duration"),
            (OBSERVE_PAUSE, "pause"),
            (OBSERVE_VOLUME, "volume"),
            (OBSERVE_SPEED, "speed"),
        ] {
            let request_id = self.allocate_request_id()?;
            let request = MpvRequest::observe_property(request_id, observer_id, property)
                .map_err(|_| invalid_command())?;
            if let Err(error) = self.send_checked(request).await {
                self.terminate_session().await;
                return Err(error);
            }
        }
        Ok(())
    }

    fn allocate_request_id(&mut self) -> Result<u64, PlayerError> {
        self.session
            .as_mut()
            .ok_or_else(closed_backend)?
            .request_ids
            .allocate()
            .map_err(|_| {
                PlayerError::new(
                    PlayerErrorCategory::Protocol,
                    "mpv request identifier space is exhausted",
                )
            })
    }

    async fn send_checked(&mut self, request: MpvRequest) -> Result<(), PlayerError> {
        let request_id = request.request_id();
        let send_result = self
            .session
            .as_mut()
            .ok_or_else(closed_backend)?
            .transport
            .send(&request)
            .await;
        if send_result.is_err() {
            self.reap_closed_session();
            return Err(transport_error("could not send an mpv command"));
        }

        let received =
            tokio::time::timeout(self.reply_timeout, self.receive_checked_reply(request_id)).await;
        if let Ok(result) = received {
            result
        } else {
            self.terminate_session().await;
            Err(transport_error("mpv command reply timed out"))
        }
    }

    async fn receive_checked_reply(&mut self, request_id: u64) -> Result<(), PlayerError> {
        loop {
            let received = self
                .session
                .as_mut()
                .ok_or_else(closed_backend)?
                .transport
                .receive_next_frame()
                .await;
            let Ok(frame) = received else {
                self.reap_closed_session();
                return Err(transport_error("could not read an mpv command reply"));
            };
            match frame {
                Some(Ok(MpvMessage::Reply(reply))) => {
                    if reply.request_id() == request_id {
                        return if reply.error() == "success" {
                            Ok(())
                        } else {
                            Err(PlayerError::new(
                                PlayerErrorCategory::Command,
                                "mpv rejected a playback command",
                            ))
                        };
                    }
                    self.capture_post_load_reply(&reply);
                }
                Some(Err(_)) => {}
                Some(Ok(MpvMessage::Event(event))) => {
                    self.session
                        .as_mut()
                        .ok_or_else(closed_backend)?
                        .pending_events
                        .push_back(event);
                }
                None => {
                    self.reap_closed_session();
                    return Err(closed_backend());
                }
            }
        }
    }

    fn capture_post_load_reply(&mut self, reply: &MpvReply) {
        let Some(pending) = self
            .session
            .as_mut()
            .and_then(|session| session.pending_file_loaded.as_mut())
        else {
            return;
        };
        let PostLoadSeekPhase::AwaitingReply {
            request_id,
            deadline,
        } = &pending.phase
        else {
            return;
        };
        if *request_id == reply.request_id() {
            pending.phase = if Instant::now() < *deadline {
                PostLoadSeekPhase::ReplyReceived {
                    success: reply.error() == "success",
                }
            } else {
                PostLoadSeekPhase::ReplyTimedOut
            };
        }
    }

    async fn next_mpv_event(&mut self) -> Result<MpvEvent, PlayerError> {
        loop {
            if let Some(event) = self
                .session
                .as_mut()
                .ok_or_else(closed_backend)?
                .pending_events
                .pop_front()
            {
                return Ok(event);
            }
            let received = self
                .session
                .as_mut()
                .ok_or_else(closed_backend)?
                .transport
                .receive_next_frame()
                .await;
            let Ok(frame) = received else {
                self.reap_closed_session();
                return Err(transport_error("could not read an mpv event"));
            };
            match frame {
                Some(Ok(MpvMessage::Event(event))) => return Ok(event),
                Some(Ok(MpvMessage::Reply(_)) | Err(_)) => {}
                None => {
                    self.reap_closed_session();
                    return Ok(MpvEvent::Shutdown);
                }
            }
        }
    }

    async fn advance_pending_file_loaded(&mut self) -> Result<PlayerEvent, PlayerError> {
        loop {
            let phase = self
                .session
                .as_ref()
                .and_then(|session| session.pending_file_loaded.as_ref())
                .ok_or_else(closed_backend)?
                .phase;
            match phase {
                PostLoadSeekPhase::ReadyToSend => {
                    let sent = {
                        let session = self.session.as_mut().ok_or_else(closed_backend)?;
                        let pending = session
                            .pending_file_loaded
                            .as_mut()
                            .ok_or_else(closed_backend)?;
                        pending.phase = PostLoadSeekPhase::Sending;
                        session.transport.send(&pending.request).await
                    };
                    if sent.is_err() {
                        self.terminate_session().await;
                        return Err(transport_error("could not send the mpv post-load seek"));
                    }
                    let session = self.session.as_mut().ok_or_else(closed_backend)?;
                    let pending = session
                        .pending_file_loaded
                        .as_mut()
                        .ok_or_else(closed_backend)?;
                    pending.phase = PostLoadSeekPhase::AwaitingReply {
                        request_id: pending.request.request_id(),
                        deadline: Instant::now() + self.reply_timeout,
                    };
                }
                PostLoadSeekPhase::Sending => {
                    self.terminate_session().await;
                    return Err(transport_error("the mpv post-load seek was interrupted"));
                }
                PostLoadSeekPhase::AwaitingReply {
                    request_id,
                    deadline,
                } => {
                    let received =
                        tokio::time::timeout_at(deadline, self.receive_checked_reply(request_id))
                            .await;
                    match received {
                        Ok(Ok(())) => return self.finish_pending_file_loaded().await,
                        Ok(Err(error)) => {
                            self.terminate_session().await;
                            return Err(error);
                        }
                        Err(_) => {
                            self.terminate_session().await;
                            return Err(transport_error("mpv post-load seek reply timed out"));
                        }
                    }
                }
                PostLoadSeekPhase::ReplyReceived { success: true } => {
                    return self.finish_pending_file_loaded().await;
                }
                PostLoadSeekPhase::ReplyReceived { success: false } => {
                    self.terminate_session().await;
                    return Err(PlayerError::new(
                        PlayerErrorCategory::Command,
                        "mpv rejected a playback command",
                    ));
                }
                PostLoadSeekPhase::ReplyTimedOut => {
                    self.terminate_session().await;
                    return Err(transport_error("mpv post-load seek reply timed out"));
                }
            }
        }
    }

    async fn finish_pending_file_loaded(&mut self) -> Result<PlayerEvent, PlayerError> {
        let epoch = self
            .session
            .as_mut()
            .and_then(|session| session.pending_file_loaded.take())
            .ok_or_else(closed_backend)?
            .epoch;
        let valid_active_load = self
            .session
            .as_mut()
            .and_then(|session| session.active_load.as_mut())
            .filter(|active_load| active_load.epoch == epoch);
        let Some(active_load) = valid_active_load else {
            self.terminate_session().await;
            return Err(transport_error(
                "mpv post-load seek no longer matches the active load",
            ));
        };
        active_load.file_loaded_delivered = true;
        Ok(PlayerEvent::FileLoaded { epoch })
    }

    async fn map_event(&mut self, event: MpvEvent) -> Result<Option<PlayerEvent>, PlayerError> {
        match event {
            MpvEvent::StartFile => {
                let epoch = self.next_load_epoch.ok_or_else(load_epoch_exhausted)?;
                self.next_load_epoch = epoch.checked_add(1);
                let epoch = LoadEpoch::new(epoch);
                let session = self.session.as_mut().ok_or_else(closed_backend)?;
                let start_ms = session
                    .pending_loads
                    .pop_front()
                    .and_then(|load| load.start_ms);
                session.active_load = Some(ActiveLoad {
                    epoch,
                    start_ms,
                    file_loaded_delivered: false,
                });
                session.pending_file_loaded = None;
                session.position_ms = 0;
                session.duration_ms = None;
                Ok(Some(PlayerEvent::LoadStarted { epoch }))
            }
            MpvEvent::FileLoaded => {
                let session = self.session.as_mut().ok_or_else(closed_backend)?;
                if session.pending_file_loaded.is_some() {
                    return Ok(None);
                }
                let Some(active_load) = session.active_load.as_mut() else {
                    return Ok(None);
                };
                if active_load.file_loaded_delivered {
                    return Ok(None);
                }
                let epoch = active_load.epoch;
                let start_ms = active_load.start_ms.take();
                let Some(start_ms) = start_ms else {
                    active_load.file_loaded_delivered = true;
                    return Ok(Some(PlayerEvent::FileLoaded { epoch }));
                };

                let request_id = self.allocate_request_id();
                if request_id.is_err() {
                    self.terminate_session().await;
                }
                let request_id = request_id?;
                let seconds = Duration::from_millis(start_ms).as_secs_f64();
                let number = Number::from_f64(seconds).ok_or_else(invalid_command);
                if number.is_err() {
                    self.terminate_session().await;
                }
                let request =
                    MpvRequest::set_property(request_id, "time-pos", Value::Number(number?))
                        .map_err(|_| invalid_command());
                if request.is_err() {
                    self.terminate_session().await;
                }
                self.session
                    .as_mut()
                    .ok_or_else(closed_backend)?
                    .pending_file_loaded = Some(PendingFileLoaded {
                    epoch,
                    request: request?,
                    phase: PostLoadSeekPhase::ReadyToSend,
                });
                Ok(None)
            }
            MpvEvent::PropertyChange { name, data, .. } => self.map_property(&name, &data),
            MpvEvent::EndFile { reason, error } => {
                let Some(epoch) = self
                    .session
                    .as_ref()
                    .ok_or_else(closed_backend)?
                    .active_load
                    .as_ref()
                    .map(|load| load.epoch)
                else {
                    return Ok(None);
                };
                let reason = match (reason.as_deref(), error.is_some()) {
                    (_, true) | (Some("error"), false) => PlayerEndReason::UrlRejected,
                    (Some("eof"), false) => PlayerEndReason::Natural,
                    (Some("stop"), false) => PlayerEndReason::Replaced,
                    (Some("quit"), false) => PlayerEndReason::Stopped,
                    _ => PlayerEndReason::Unknown,
                };
                Ok(Some(PlayerEvent::Ended { epoch, reason }))
            }
            MpvEvent::Shutdown => {
                self.reap_closed_session();
                Ok(Some(PlayerEvent::Shutdown))
            }
            MpvEvent::Unknown { .. } => Ok(None),
        }
    }

    fn map_property(
        &mut self,
        name: &str,
        data: &Value,
    ) -> Result<Option<PlayerEvent>, PlayerError> {
        let session = self.session.as_mut().ok_or_else(closed_backend)?;
        let Some(epoch) = session.active_load.as_ref().map(|load| load.epoch) else {
            return Ok(None);
        };
        match name {
            "time-pos" => {
                let Some(position_ms) = seconds_value_to_millis(data) else {
                    return Ok(None);
                };
                session.position_ms = position_ms;
                Ok(Some(PlayerEvent::Progress {
                    epoch,
                    position_ms,
                    duration_ms: session.duration_ms,
                }))
            }
            "duration" => {
                session.duration_ms = seconds_value_to_millis(data);
                Ok(Some(PlayerEvent::Progress {
                    epoch,
                    position_ms: session.position_ms,
                    duration_ms: session.duration_ms,
                }))
            }
            "pause" => Ok(data
                .as_bool()
                .map(|paused| PlayerEvent::PauseChanged { epoch, paused })),
            "volume" => Ok(finite_property(data).map(PlayerEvent::VolumeChanged)),
            "speed" => Ok(finite_property(data).map(PlayerEvent::SpeedChanged)),
            _ => Ok(None),
        }
    }

    fn reap_closed_session(&mut self) {
        if let Some(mut session) = self.session.take() {
            let _ = session.child.try_wait();
        }
    }

    async fn terminate_session(&mut self) {
        if let Some(mut session) = self.session.take() {
            terminate(&mut session.child).await;
        }
    }
}

#[async_trait]
impl PlayerBackend for MpvBackend {
    async fn load(&mut self, url: &Url, start_ms: Option<u64>) -> Result<(), PlayerError> {
        self.ensure_session().await?;
        let request_id = self.allocate_request_id();
        if request_id.is_err() {
            self.terminate_session().await;
        }
        let request_id = request_id?;
        let request = MpvRequest::loadfile(request_id, url.as_str()).map_err(|_| invalid_command());
        if request.is_err() {
            self.terminate_session().await;
        }
        let request = request?;
        self.session
            .as_mut()
            .ok_or_else(closed_backend)?
            .pending_loads
            .push_back(PendingLoad { start_ms });
        if let Err(error) = self.send_checked(request).await {
            self.terminate_session().await;
            return Err(error);
        }
        Ok(())
    }

    async fn reset_session(&mut self) {
        self.terminate_session().await;
    }

    async fn set_paused(&mut self, paused: bool) -> Result<(), PlayerError> {
        self.ensure_session().await?;
        let request_id = self.allocate_request_id()?;
        let request = MpvRequest::set_property(request_id, "pause", Value::Bool(paused))
            .map_err(|_| invalid_command())?;
        self.send_checked(request).await
    }

    async fn seek_relative(&mut self, seconds: i64) -> Result<(), PlayerError> {
        self.ensure_session().await?;
        let seconds = seconds
            .to_string()
            .parse::<f64>()
            .map_err(|_| invalid_command())?;
        let request_id = self.allocate_request_id()?;
        let request =
            MpvRequest::seek_relative(request_id, seconds).map_err(|_| invalid_command())?;
        self.send_checked(request).await
    }

    async fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        self.ensure_session().await?;
        let request_id = self.allocate_request_id()?;
        let request = MpvRequest::set_volume(request_id, volume).map_err(|_| invalid_command())?;
        self.send_checked(request).await
    }

    async fn set_speed(&mut self, speed: f64) -> Result<(), PlayerError> {
        self.ensure_session().await?;
        let request_id = self.allocate_request_id()?;
        let number = Number::from_f64(speed).ok_or_else(invalid_command)?;
        let request = MpvRequest::set_property(request_id, "speed", Value::Number(number))
            .map_err(|_| invalid_command())?;
        self.send_checked(request).await
    }

    async fn next_event(&mut self) -> Result<PlayerEvent, PlayerError> {
        loop {
            if self
                .session
                .as_ref()
                .is_some_and(|session| session.pending_file_loaded.is_some())
            {
                return self.advance_pending_file_loaded().await;
            }
            let event = self.next_mpv_event().await?;
            if let Some(event) = self.map_event(event).await? {
                return Ok(event);
            }
        }
    }

    async fn shutdown(&mut self) -> Result<(), PlayerError> {
        let Some(mut session) = self.session.take() else {
            return Ok(());
        };
        let request_id = session.request_ids.allocate().map_err(|_| {
            PlayerError::new(
                PlayerErrorCategory::Protocol,
                "mpv request identifier space is exhausted",
            )
        })?;
        let request = MpvRequest::quit(request_id).map_err(|_| invalid_command())?;
        let _ = session.transport.send(&request).await;
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, session.child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => terminate(&mut session.child).await,
        }
        Ok(())
    }
}

impl fmt::Debug for MpvBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MpvBackend")
            .field("executable", &self.executable)
            .field("session_active", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MpvSession")
            .field("endpoint", &self.endpoint)
            .field("pending_events", &self.pending_events.len())
            .field("pending_loads", &self.pending_loads.len())
            .field("file_loaded_pending", &self.pending_file_loaded.is_some())
            .field(
                "active_epoch",
                &self.active_load.as_ref().map(|load| load.epoch),
            )
            .finish_non_exhaustive()
    }
}

fn finite_property(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn seconds_value_to_millis(value: &Value) -> Option<u64> {
    let seconds = finite_property(value)?;
    if seconds < 0.0 {
        return None;
    }
    let millis = Duration::from_secs_f64(seconds).as_millis();
    Some(u64::try_from(millis).unwrap_or(u64::MAX))
}

async fn terminate(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn invalid_command() -> PlayerError {
    PlayerError::new(
        PlayerErrorCategory::Command,
        "invalid value for an mpv playback command",
    )
}

fn closed_backend() -> PlayerError {
    PlayerError::new(
        PlayerErrorCategory::Closed,
        "the mpv playback process is not connected",
    )
}

fn transport_error(message: &'static str) -> PlayerError {
    PlayerError::new(PlayerErrorCategory::Protocol, message)
}

fn backend_wait_error() -> PlayerError {
    PlayerError::new(
        PlayerErrorCategory::Backend,
        "could not inspect the mpv playback process",
    )
}

fn load_epoch_exhausted() -> PlayerError {
    PlayerError::new(
        PlayerErrorCategory::Protocol,
        "mpv load epoch space is exhausted",
    )
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use futures::FutureExt as _;
    use tokio::{
        io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, duplex},
        sync::oneshot,
    };

    use super::*;

    type TestResult = Result<(), Box<dyn Error>>;
    const CHILD_FIXTURE_ENV: &str = "YTERMUSIC_MPV_TIMEOUT_CHILD";

    #[test]
    fn mpv_timeout_child_fixture() {
        if std::env::var_os(CHILD_FIXTURE_ENV).is_none() {
            return;
        }
        loop {
            std::thread::park();
        }
    }

    fn child_fixture() -> Result<Child, io::Error> {
        let executable = std::env::current_exe()?;
        tokio::process::Command::new(executable)
            .arg("--exact")
            .arg("player::mpv::tests::mpv_timeout_child_fixture")
            .env(CHILD_FIXTURE_ENV, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
    }

    fn event_backend(
        stream: DuplexStream,
        reply_timeout: Duration,
        start_ms: Option<u64>,
        pending_events: impl IntoIterator<Item = MpvEvent>,
    ) -> Result<MpvBackend, io::Error> {
        Ok(MpvBackend {
            executable: std::env::current_exe()?,
            connector: Arc::new(NativeMpvConnector),
            reply_timeout,
            next_load_epoch: Some(2),
            session: Some(Session {
                endpoint: IpcEndpoint::native()?,
                child: child_fixture()?,
                transport: MpvTransport::new(Box::new(stream), MAX_MPV_LINE_BYTES)?,
                request_ids: RequestIdAllocator::default(),
                pending_events: pending_events.into_iter().collect(),
                pending_loads: VecDeque::new(),
                active_load: Some(ActiveLoad {
                    epoch: LoadEpoch::new(1),
                    start_ms,
                    file_loaded_delivered: false,
                }),
                pending_file_loaded: None,
                position_ms: 0,
                duration_ms: None,
            }),
        })
    }

    async fn serve_epoch_peer(peer: DuplexStream) -> Result<(), io::Error> {
        let (read, mut write) = tokio::io::split(peer);
        let mut requests = BufReader::new(read).lines();

        let first = requests
            .next_line()
            .await?
            .ok_or_else(|| io::Error::other("missing first load request"))?;
        assert!(first.contains("\"request_id\":1"));
        write
            .write_all(
                b"{\"event\":\"start-file\"}\n\
                  {\"error\":\"success\",\"request_id\":1}\n",
            )
            .await?;

        let second = requests
            .next_line()
            .await?
            .ok_or_else(|| io::Error::other("missing second load request"))?;
        assert!(second.contains("\"request_id\":2"));
        write
            .write_all(
                b"{\"event\":\"file-loaded\"}\n\
                  {\"event\":\"property-change\",\"id\":1,\"name\":\"time-pos\",\"data\":12.5}\n\
                  {\"event\":\"end-file\",\"reason\":\"eof\"}\n\
                  {\"event\":\"start-file\"}\n\
                  {\"event\":\"file-loaded\"}\n\
                  {\"error\":\"success\",\"request_id\":2}\n",
            )
            .await?;

        let first_seek = requests
            .next_line()
            .await?
            .ok_or_else(|| io::Error::other("missing first seek request"))?;
        let first_seek: Value = serde_json::from_str(&first_seek).map_err(io::Error::other)?;
        assert_eq!(first_seek["command"][2], Value::from(1.25));
        write
            .write_all(b"{\"error\":\"success\",\"request_id\":3}\n")
            .await?;

        let second_seek = requests
            .next_line()
            .await?
            .ok_or_else(|| io::Error::other("missing second seek request"))?;
        let second_seek: Value = serde_json::from_str(&second_seek).map_err(io::Error::other)?;
        assert_eq!(second_seek["command"][2], Value::from(2.5));
        write
            .write_all(b"{\"error\":\"success\",\"request_id\":4}\n")
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_command_times_out_and_tears_down_the_session() -> TestResult {
        let reply_timeout = Duration::from_millis(250);
        let (stream, _silent_peer) = duplex(4_096);
        let backend = MpvBackend {
            executable: std::env::current_exe()?,
            connector: Arc::new(NativeMpvConnector),
            reply_timeout,
            next_load_epoch: Some(1),
            session: Some(Session {
                endpoint: IpcEndpoint::native()?,
                child: child_fixture()?,
                transport: MpvTransport::new(Box::new(stream), MAX_MPV_LINE_BYTES)?,
                request_ids: RequestIdAllocator::default(),
                pending_events: VecDeque::new(),
                pending_loads: VecDeque::new(),
                active_load: None,
                pending_file_loaded: None,
                position_ms: 0,
                duration_ms: None,
            }),
        };

        let command = tokio::spawn(async move {
            let mut backend = backend;
            let result = backend.set_volume(50.0).await;
            (result, backend)
        });
        tokio::task::yield_now().await;
        assert!(!command.is_finished());

        tokio::time::advance(reply_timeout).await;
        let (result, backend) = command.await?;
        let error = result
            .err()
            .ok_or_else(|| io::Error::other("an unanswered mpv command unexpectedly succeeded"))?;
        assert_eq!(error.category(), PlayerErrorCategory::Protocol);
        assert_eq!(error.message(), "mpv command reply timed out");
        assert!(backend.session.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn queued_events_are_tagged_by_start_file_order_across_load_replies() -> TestResult {
        let (stream, peer) = duplex(8_192);
        let mut backend = MpvBackend {
            executable: std::env::current_exe()?,
            connector: Arc::new(NativeMpvConnector),
            reply_timeout: Duration::from_secs(1),
            next_load_epoch: Some(1),
            session: Some(Session {
                endpoint: IpcEndpoint::native()?,
                child: child_fixture()?,
                transport: MpvTransport::new(Box::new(stream), MAX_MPV_LINE_BYTES)?,
                request_ids: RequestIdAllocator::default(),
                pending_events: VecDeque::new(),
                pending_loads: VecDeque::new(),
                active_load: None,
                pending_file_loaded: None,
                position_ms: 0,
                duration_ms: None,
            }),
        };
        let peer_task = tokio::spawn(serve_epoch_peer(peer));

        backend
            .load(&Url::parse("https://media.invalid/a")?, Some(1_250))
            .await?;
        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::LoadStarted {
                epoch: LoadEpoch::new(1),
            }
        );

        backend
            .load(&Url::parse("https://media.invalid/b")?, Some(2_500))
            .await?;
        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::FileLoaded {
                epoch: LoadEpoch::new(1),
            }
        );
        assert!(matches!(
            backend.next_event().await?,
            PlayerEvent::Progress {
                epoch,
                position_ms: 12_500,
                ..
            } if epoch == LoadEpoch::new(1)
        ));
        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::Ended {
                epoch: LoadEpoch::new(1),
                reason: PlayerEndReason::Natural,
            }
        );
        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::LoadStarted {
                epoch: LoadEpoch::new(2),
            }
        );
        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::FileLoaded {
                epoch: LoadEpoch::new(2),
            }
        );
        peer_task.await??;
        backend.terminate_session().await;
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_file_loaded_seek_resumes_without_resending_or_losing_the_event() -> TestResult
    {
        let (stream, peer) = duplex(4_096);
        let mut backend = event_backend(
            stream,
            Duration::from_secs(1),
            Some(1_250),
            [MpvEvent::FileLoaded],
        )?;
        let (seek_sent, seek_observed) = oneshot::channel();
        let (release_reply, reply_released) = oneshot::channel();
        let peer_task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(peer);
            let mut requests = BufReader::new(read).lines();
            let request = requests
                .next_line()
                .await?
                .ok_or_else(|| io::Error::other("missing post-load seek request"))?;
            let request: Value = serde_json::from_str(&request).map_err(io::Error::other)?;
            assert_eq!(request["request_id"], Value::from(1));
            assert_eq!(request["command"][0], Value::from("set_property"));
            assert_eq!(request["command"][1], Value::from("time-pos"));
            assert_eq!(request["command"][2], Value::from(1.25));
            seek_sent
                .send(())
                .map_err(|()| io::Error::other("seek observer was dropped"))?;
            reply_released
                .await
                .map_err(|_| io::Error::other("seek reply release was dropped"))?;
            write
                .write_all(b"{\"error\":\"success\",\"request_id\":1}\n")
                .await?;

            let command = requests
                .next_line()
                .await?
                .ok_or_else(|| io::Error::other("missing intervening volume request"))?;
            let command: Value = serde_json::from_str(&command).map_err(io::Error::other)?;
            assert_eq!(command["request_id"], Value::from(2));
            assert_eq!(command["command"][0], Value::from("set_property"));
            assert_eq!(command["command"][1], Value::from("volume"));
            assert_eq!(command["command"][2], Value::from(50.0));
            write
                .write_all(
                    b"{\"error\":\"success\",\"request_id\":2}\n\
                      {\"event\":\"property-change\",\"id\":3,\"name\":\"pause\",\"data\":false}\n",
                )
                .await
        });

        let mut interrupted = Box::pin(backend.next_event());
        tokio::select! {
            result = &mut interrupted => {
                let _ = result?;
                return Err(io::Error::other("FileLoaded completed before the seek reply").into());
            }
            observed = seek_observed => {
                observed?;
            }
        }
        drop(interrupted);
        release_reply
            .send(())
            .map_err(|()| io::Error::other("seek reply task was dropped"))?;
        backend.set_volume(50.0).await?;

        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::FileLoaded {
                epoch: LoadEpoch::new(1),
            }
        );
        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::PauseChanged {
                epoch: LoadEpoch::new(1),
                paused: false,
            }
        );
        peer_task.await??;
        backend.terminate_session().await;
        Ok(())
    }

    #[tokio::test]
    async fn rejected_file_loaded_seek_tears_down_the_session() -> TestResult {
        let (stream, peer) = duplex(1_024);
        let mut backend = event_backend(
            stream,
            Duration::from_secs(1),
            Some(1_250),
            [MpvEvent::FileLoaded],
        )?;
        let peer_task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(peer);
            let request = BufReader::new(read)
                .lines()
                .next_line()
                .await?
                .ok_or_else(|| io::Error::other("missing post-load seek request"))?;
            assert!(request.contains("\"request_id\":1"));
            write
                .write_all(b"{\"error\":\"invalid parameter\",\"request_id\":1}\n")
                .await
        });

        let error =
            backend.next_event().await.err().ok_or_else(|| {
                io::Error::other("rejected post-load seek unexpectedly succeeded")
            })?;
        assert_eq!(error.category(), PlayerErrorCategory::Command);
        assert!(backend.session.is_none());
        peer_task.await??;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_file_loaded_seek_tears_down_the_session() -> TestResult {
        let reply_timeout = Duration::from_millis(250);
        let (stream, peer) = duplex(1_024);
        let backend = event_backend(stream, reply_timeout, Some(1_250), [MpvEvent::FileLoaded])?;
        let command = tokio::spawn(async move {
            let mut backend = backend;
            let result = backend.next_event().await;
            (result, backend)
        });
        let mut peer = BufReader::new(peer).lines();
        let request = peer
            .next_line()
            .await?
            .ok_or_else(|| io::Error::other("missing post-load seek request"))?;
        assert!(request.contains("\"request_id\":1"));
        assert!(!command.is_finished());

        tokio::time::advance(reply_timeout).await;
        let (result, backend) = command.await?;
        let error = result
            .err()
            .ok_or_else(|| io::Error::other("timed-out post-load seek unexpectedly succeeded"))?;
        assert_eq!(error.category(), PlayerErrorCategory::Protocol);
        assert!(backend.session.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn file_loaded_without_a_start_position_is_emitted_once_without_a_command() -> TestResult
    {
        let (stream, _peer) = duplex(1_024);
        let mut backend = event_backend(
            stream,
            Duration::from_secs(1),
            None,
            [
                MpvEvent::FileLoaded,
                MpvEvent::FileLoaded,
                MpvEvent::PropertyChange {
                    observer_id: OBSERVE_PAUSE,
                    name: "pause".to_owned(),
                    data: Value::Bool(false),
                },
            ],
        )?;

        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::FileLoaded {
                epoch: LoadEpoch::new(1),
            }
        );
        assert_eq!(
            backend.next_event().await?,
            PlayerEvent::PauseChanged {
                epoch: LoadEpoch::new(1),
                paused: false,
            }
        );
        assert_eq!(
            backend.allocate_request_id()?,
            1,
            "no post-load seek request should have consumed an ID"
        );
        backend.terminate_session().await;
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_partial_file_loaded_seek_tears_down_without_resending() -> TestResult {
        let (stream, _peer) = duplex(1);
        let mut backend = event_backend(
            stream,
            Duration::from_secs(1),
            Some(1_250),
            [MpvEvent::FileLoaded],
        )?;

        let mut interrupted = Box::pin(backend.next_event());
        assert!(interrupted.as_mut().now_or_never().is_none());
        drop(interrupted);

        let error =
            backend.next_event().await.err().ok_or_else(|| {
                io::Error::other("interrupted post-load seek unexpectedly resumed")
            })?;
        assert_eq!(error.category(), PlayerErrorCategory::Protocol);
        assert!(backend.session.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn new_epoch_duration_does_not_reuse_the_previous_position() -> TestResult {
        let (stream, _peer) = duplex(1_024);
        let mut backend = event_backend(stream, Duration::from_secs(1), None, [])?;
        let session = backend.session.as_mut().ok_or_else(closed_backend)?;
        session.position_ms = 99_500;
        session.duration_ms = Some(100_000);

        assert_eq!(
            backend.map_event(MpvEvent::StartFile).await?,
            Some(PlayerEvent::LoadStarted {
                epoch: LoadEpoch::new(2),
            })
        );
        assert_eq!(
            backend
                .map_event(MpvEvent::PropertyChange {
                    observer_id: OBSERVE_DURATION,
                    name: "duration".to_owned(),
                    data: Value::from(200.0),
                })
                .await?,
            Some(PlayerEvent::Progress {
                epoch: LoadEpoch::new(2),
                position_ms: 0,
                duration_ms: Some(200_000),
            })
        );
        backend.terminate_session().await;
        Ok(())
    }

    #[tokio::test]
    async fn new_epoch_null_time_position_does_not_reuse_the_previous_duration() -> TestResult {
        let (stream, _peer) = duplex(1_024);
        let mut backend = event_backend(stream, Duration::from_secs(1), None, [])?;
        let session = backend.session.as_mut().ok_or_else(closed_backend)?;
        session.position_ms = 99_500;
        session.duration_ms = Some(100_000);

        assert_eq!(
            backend.map_event(MpvEvent::StartFile).await?,
            Some(PlayerEvent::LoadStarted {
                epoch: LoadEpoch::new(2),
            })
        );
        assert_eq!(
            backend
                .map_event(MpvEvent::PropertyChange {
                    observer_id: OBSERVE_TIME_POS,
                    name: "time-pos".to_owned(),
                    data: Value::Null,
                })
                .await?,
            None
        );
        assert_eq!(
            backend
                .map_event(MpvEvent::PropertyChange {
                    observer_id: OBSERVE_TIME_POS,
                    name: "time-pos".to_owned(),
                    data: Value::from(1.0),
                })
                .await?,
            Some(PlayerEvent::Progress {
                epoch: LoadEpoch::new(2),
                position_ms: 1_000,
                duration_ms: None,
            })
        );
        backend.terminate_session().await;
        Ok(())
    }

    #[tokio::test]
    async fn load_epochs_remain_monotonic_when_the_mpv_session_is_replaced() -> TestResult {
        let (first_stream, _first_peer) = duplex(1_024);
        let mut backend = MpvBackend {
            executable: std::env::current_exe()?,
            connector: Arc::new(NativeMpvConnector),
            reply_timeout: Duration::from_secs(1),
            next_load_epoch: Some(1),
            session: Some(Session {
                endpoint: IpcEndpoint::native()?,
                child: child_fixture()?,
                transport: MpvTransport::new(Box::new(first_stream), MAX_MPV_LINE_BYTES)?,
                request_ids: RequestIdAllocator::default(),
                pending_events: VecDeque::new(),
                pending_loads: VecDeque::new(),
                active_load: None,
                pending_file_loaded: None,
                position_ms: 0,
                duration_ms: None,
            }),
        };
        assert_eq!(
            backend.map_event(MpvEvent::StartFile).await?,
            Some(PlayerEvent::LoadStarted {
                epoch: LoadEpoch::new(1),
            })
        );
        backend.terminate_session().await;

        let (second_stream, _second_peer) = duplex(1_024);
        backend.session = Some(Session {
            endpoint: IpcEndpoint::native()?,
            child: child_fixture()?,
            transport: MpvTransport::new(Box::new(second_stream), MAX_MPV_LINE_BYTES)?,
            request_ids: RequestIdAllocator::default(),
            pending_events: VecDeque::new(),
            pending_loads: VecDeque::new(),
            active_load: None,
            pending_file_loaded: None,
            position_ms: 0,
            duration_ms: None,
        });
        assert_eq!(
            backend.map_event(MpvEvent::StartFile).await?,
            Some(PlayerEvent::LoadStarted {
                epoch: LoadEpoch::new(2),
            })
        );
        backend.terminate_session().await;
        Ok(())
    }

    #[tokio::test]
    async fn rejected_load_discards_its_queued_start_file_with_the_session() -> TestResult {
        let (stream, peer) = duplex(1_024);
        let mut backend = MpvBackend {
            executable: std::env::current_exe()?,
            connector: Arc::new(NativeMpvConnector),
            reply_timeout: Duration::from_secs(1),
            next_load_epoch: Some(1),
            session: Some(Session {
                endpoint: IpcEndpoint::native()?,
                child: child_fixture()?,
                transport: MpvTransport::new(Box::new(stream), MAX_MPV_LINE_BYTES)?,
                request_ids: RequestIdAllocator::default(),
                pending_events: VecDeque::new(),
                pending_loads: VecDeque::new(),
                active_load: None,
                pending_file_loaded: None,
                position_ms: 0,
                duration_ms: None,
            }),
        };
        let peer_task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(peer);
            let request = BufReader::new(read)
                .lines()
                .next_line()
                .await?
                .ok_or_else(|| io::Error::other("missing load request"))?;
            assert!(request.contains("\"request_id\":1"));
            write
                .write_all(
                    b"{\"event\":\"start-file\"}\n\
                      {\"error\":\"invalid parameter\",\"request_id\":1}\n",
                )
                .await
        });

        let error = backend
            .load(&Url::parse("https://media.invalid/rejected")?, None)
            .await
            .err()
            .ok_or_else(|| io::Error::other("mpv command rejection unexpectedly succeeded"))?;

        assert_eq!(error.category(), PlayerErrorCategory::Command);
        assert!(backend.session.is_none());
        peer_task.await??;
        Ok(())
    }

    #[tokio::test]
    async fn pre_send_load_failure_discards_the_session_event_queue() -> TestResult {
        let (stream, _peer) = duplex(1_024);
        let mut request_ids = RequestIdAllocator::starting_at(i64::MAX as u64)?;
        assert_eq!(request_ids.allocate()?, i64::MAX as u64);
        let mut backend = MpvBackend {
            executable: std::env::current_exe()?,
            connector: Arc::new(NativeMpvConnector),
            reply_timeout: Duration::from_secs(1),
            next_load_epoch: Some(1),
            session: Some(Session {
                endpoint: IpcEndpoint::native()?,
                child: child_fixture()?,
                transport: MpvTransport::new(Box::new(stream), MAX_MPV_LINE_BYTES)?,
                request_ids,
                pending_events: VecDeque::new(),
                pending_loads: VecDeque::new(),
                active_load: None,
                pending_file_loaded: None,
                position_ms: 0,
                duration_ms: None,
            }),
        };

        let error = backend
            .load(&Url::parse("https://media.invalid/not-sent")?, None)
            .await
            .err()
            .ok_or_else(|| io::Error::other("request ID exhaustion unexpectedly succeeded"))?;

        assert_eq!(error.category(), PlayerErrorCategory::Protocol);
        assert!(backend.session.is_none());
        Ok(())
    }
}
