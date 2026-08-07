use std::{
    cmp::Ordering,
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
    AuthIdentity, CookieFile, PreviewStreamUrl, ResolveError, ResolveErrorCategory, ResolvePolicy,
    ResolvedAudioUrlError, ResolvedStream, Resolver,
};
use crate::{
    diagnostics::sanitize,
    domain::{MediaItem, MediaKind},
    process::{CommandSpec, ProcessError, ProcessLimits, ProcessOutput, ProcessRunner},
};

const SUPPORTED_PROVIDER: &str = "youtube-music";
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_STDOUT_BYTES: usize = 4 * 1_024 * 1_024;
const MAX_STDERR_BYTES: usize = 16 * 1_024;
const MAX_ERROR_CHARS: usize = 512;
const DEFAULT_CACHE_CAPACITY: usize = 256;
const MAX_CONCURRENT_PROCESSES: usize = 2;
const FIRST_INVALID_U64: f64 = 18_446_744_073_709_551_616.0;
const MAX_PREVIEW_FORMATS: usize = 512;
const MAX_PREVIEW_WIDTH: u64 = 640;
const MAX_PREVIEW_HEIGHT: u64 = 360;
const MAX_PREVIEW_FPS: f64 = 30.0;

pub trait ResolverClock: Send + Sync {
    fn now(&self) -> OffsetDateTime;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemResolverClock;

impl ResolverClock for SystemResolverClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum CacheAuth {
    Anonymous,
    Authenticated(AuthIdentity),
}

impl Ord for CacheAuth {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Anonymous, Self::Anonymous) => Ordering::Equal,
            (Self::Anonymous, Self::Authenticated(_)) => Ordering::Less,
            (Self::Authenticated(_), Self::Anonymous) => Ordering::Greater,
            (Self::Authenticated(left), Self::Authenticated(right)) => {
                left.cache_key().cmp(right.cache_key())
            }
        }
    }
}

impl PartialOrd for CacheAuth {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct CacheKey {
    video_id: String,
    preview_eligible: bool,
    auth: CacheAuth,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum FlightEpoch {
    Missing,
    Refresh(u128),
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct FlightKey {
    cache: CacheKey,
    epoch: FlightEpoch,
}

struct FlightState {
    result: Option<Result<ResolvedStream, ResolveError>>,
    waiters: HashMap<u128, CancellationToken>,
    next_waiter_id: u128,
    abandoned: bool,
}

struct Flight {
    state: Mutex<FlightState>,
    completed: Notify,
    cancel: CancellationToken,
    done: CancellationToken,
}

impl Flight {
    fn with_first_waiter(cancel: CancellationToken) -> Self {
        let mut waiters = HashMap::new();
        waiters.insert(0, cancel);
        Self {
            state: Mutex::new(FlightState {
                result: None,
                waiters,
                next_waiter_id: 1,
                abandoned: false,
            }),
            completed: Notify::new(),
            cancel: CancellationToken::new(),
            done: CancellationToken::new(),
        }
    }

    fn try_add_waiter(&self, cancel: CancellationToken) -> Option<u128> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.result.is_some() || state.abandoned {
            return None;
        }
        let waiter_id = state.next_waiter_id;
        state.next_waiter_id = state.next_waiter_id.saturating_add(1);
        state.waiters.insert(waiter_id, cancel);
        Some(waiter_id)
    }

    fn leave_waiter(&self, waiter_id: u128) {
        let cancel = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.waiters.remove(&waiter_id).is_none() {
                return;
            }
            if state.waiters.is_empty() && state.result.is_none() {
                state.abandoned = true;
                true
            } else {
                false
            }
        };
        if cancel {
            self.cancel.cancel();
        }
    }

    fn ensure_needed(&self) -> Result<(), ResolveError> {
        let cancel = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.waiters.retain(|_, token| !token.is_cancelled());
            if state.waiters.is_empty() && state.result.is_none() {
                state.abandoned = true;
                true
            } else {
                false
            }
        };
        if cancel {
            self.cancel.cancel();
            Err(cancelled())
        } else {
            active(&self.cancel)
        }
    }

    fn publish(&self, result: Result<ResolvedStream, ResolveError>) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.result.is_some() {
                return;
            }
            state.result = Some(result);
        }
        self.done.cancel();
        self.completed.notify_waiters();
    }

    async fn result(&self) -> Result<ResolvedStream, ResolveError> {
        loop {
            let completed = self.completed.notified();
            if let Some(result) = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .result
                .clone()
            {
                return result;
            }
            completed.await;
        }
    }
}

struct FlightWaiter {
    flight: Arc<Flight>,
    waiter_id: u128,
    active: bool,
}

impl FlightWaiter {
    fn new(flight: Arc<Flight>, waiter_id: u128, cancel: &CancellationToken) -> Self {
        let watcher_flight = flight.clone();
        let watcher_cancel = cancel.clone();
        let done = flight.done.clone();
        let _cancellation_watcher = tokio::spawn(async move {
            tokio::select! {
                biased;
                () = watcher_cancel.cancelled() => {
                    watcher_flight.leave_waiter(waiter_id);
                }
                () = done.cancelled() => {}
            }
        });
        Self {
            flight,
            waiter_id,
            active: true,
        }
    }

    async fn wait(mut self, cancel: CancellationToken) -> Result<ResolvedStream, ResolveError> {
        let flight = self.flight.clone();
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(cancelled()),
            result = flight.result() => result,
        };
        self.leave();
        result
    }

    fn leave(&mut self) {
        if self.active {
            self.active = false;
            self.flight.leave_waiter(self.waiter_id);
        }
    }
}

impl Drop for FlightWaiter {
    fn drop(&mut self) {
        self.leave();
    }
}

#[derive(Clone)]
struct CacheEntry {
    stream: ResolvedStream,
    generation: u128,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    next_generation: u128,
}

impl CacheState {
    fn prune(&mut self, now: OffsetDateTime, ttl: Duration) {
        self.entries
            .retain(|_, entry| is_live(now, entry.stream.resolved_at, ttl));
    }

    fn entry(&mut self, key: &CacheKey, now: OffsetDateTime, ttl: Duration) -> Option<CacheEntry> {
        self.prune(now, ttl);
        self.entries.get(key).cloned()
    }

    fn insert(
        &mut self,
        key: CacheKey,
        stream: ResolvedStream,
        now: OffsetDateTime,
        ttl: Duration,
        capacity: usize,
    ) {
        self.prune(now, ttl);
        if capacity == 0 || !is_live(now, stream.resolved_at, ttl) {
            return;
        }

        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        if !self.entries.contains_key(&key)
            && self.entries.len() >= capacity
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by(|(left_key, left), (right_key, right)| {
                    left.generation
                        .cmp(&right.generation)
                        .then_with(|| left_key.cmp(right_key))
                })
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, CacheEntry { stream, generation });
    }
}

#[derive(Clone)]
pub struct YtDlpResolver {
    program: PathBuf,
    runner: Arc<dyn ProcessRunner>,
    clock: Arc<dyn ResolverClock>,
    ttl: Duration,
    cache_capacity: usize,
    cache: Arc<Mutex<CacheState>>,
    process_gate: Arc<Semaphore>,
    in_flight: Arc<Mutex<HashMap<FlightKey, Arc<Flight>>>>,
}

impl YtDlpResolver {
    #[must_use]
    pub fn new(
        program: impl Into<PathBuf>,
        runner: Arc<dyn ProcessRunner>,
        clock: Arc<dyn ResolverClock>,
        ttl: Duration,
    ) -> Self {
        Self::with_cache_capacity(program, runner, clock, ttl, DEFAULT_CACHE_CAPACITY)
    }

    #[must_use]
    pub fn with_cache_capacity(
        program: impl Into<PathBuf>,
        runner: Arc<dyn ProcessRunner>,
        clock: Arc<dyn ResolverClock>,
        ttl: Duration,
        cache_capacity: usize,
    ) -> Self {
        Self {
            program: program.into(),
            runner,
            clock,
            ttl,
            cache_capacity,
            cache: Arc::new(Mutex::new(CacheState::default())),
            process_gate: Arc::new(Semaphore::new(MAX_CONCURRENT_PROCESSES)),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn cache_key(item: &MediaItem, auth: Option<&CookieFile>) -> CacheKey {
        CacheKey {
            video_id: item.id.video_id.clone(),
            preview_eligible: item.kind == MediaKind::Video,
            auth: auth.map_or(CacheAuth::Anonymous, |cookie| {
                CacheAuth::Authenticated(cookie.identity().clone())
            }),
        }
    }

    fn cache_entry(&self, key: &CacheKey, now: OffsetDateTime) -> Option<CacheEntry> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key, now, self.ttl)
    }

    fn cache_success(
        &self,
        key: CacheKey,
        stream: ResolvedStream,
        now: OffsetDateTime,
        cancel: &CancellationToken,
    ) -> Result<(), ResolveError> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active(cancel)?;
        cache.insert(key, stream, now, self.ttl, self.cache_capacity);
        Ok(())
    }

    async fn process_permit(
        &self,
        cancel: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit, ResolveError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(cancelled()),
            permit = self.process_gate.clone().acquire_owned() => permit.map_err(|_| {
                ResolveError::new(
                    ResolveErrorCategory::Process,
                    "the media resolver process gate is unavailable",
                )
            }),
        }
    }

    fn join_flight(&self, key: &FlightKey, cancel: &CancellationToken) -> (FlightWaiter, bool) {
        let mut flights = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(flight) = flights.get(key).cloned() {
            if let Some(waiter_id) = flight.try_add_waiter(cancel.clone()) {
                return (FlightWaiter::new(flight, waiter_id, cancel), false);
            }
            if flights
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, &flight))
            {
                flights.remove(key);
            }
        }

        if key.epoch == FlightEpoch::Missing {
            loop {
                let compatible = flights
                    .iter()
                    .filter_map(|(candidate_key, flight)| match candidate_key.epoch {
                        FlightEpoch::Refresh(generation) if candidate_key.cache == key.cache => {
                            Some((generation, candidate_key.clone(), flight.clone()))
                        }
                        FlightEpoch::Missing | FlightEpoch::Refresh(_) => None,
                    })
                    .max_by_key(|(generation, _, _)| *generation);
                let Some((_, candidate_key, flight)) = compatible else {
                    break;
                };
                if let Some(waiter_id) = flight.try_add_waiter(cancel.clone()) {
                    return (FlightWaiter::new(flight, waiter_id, cancel), false);
                }
                if flights
                    .get(&candidate_key)
                    .is_some_and(|current| Arc::ptr_eq(current, &flight))
                {
                    flights.remove(&candidate_key);
                }
            }
        }

        let flight = Arc::new(Flight::with_first_waiter(cancel.clone()));
        flights.insert(key.clone(), flight.clone());
        (FlightWaiter::new(flight, 0, cancel), true)
    }

    fn start_flight(
        &self,
        key: &FlightKey,
        flight: &Arc<Flight>,
        item: MediaItem,
        auth: Option<CookieFile>,
    ) {
        let resolver = self.clone();
        let task_flight = flight.clone();
        let task_key = key.clone();
        let _flight_task = tokio::spawn(async move {
            let result = resolver
                .execute_flight(&item, auth.as_ref(), task_key.epoch, &task_flight)
                .await;
            task_flight.publish(result);
            resolver.remove_flight(&task_key, &task_flight);
        });
    }

    fn remove_flight(&self, key: &FlightKey, completed: &Arc<Flight>) {
        let mut flights = self
            .in_flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if flights
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, completed))
        {
            flights.remove(key);
        }
    }

    async fn execute_flight(
        &self,
        item: &MediaItem,
        auth: Option<&CookieFile>,
        epoch: FlightEpoch,
        flight: &Flight,
    ) -> Result<ResolvedStream, ResolveError> {
        flight.ensure_needed()?;
        let _permit = self.process_permit(&flight.cancel).await?;
        flight.ensure_needed()?;

        let key = Self::cache_key(item, auth);
        let current = self.cache_entry(&key, self.clock.now());
        flight.ensure_needed()?;
        match epoch {
            FlightEpoch::Missing => {
                if let Some(entry) = current {
                    flight.ensure_needed()?;
                    return Ok(entry.stream);
                }
            }
            FlightEpoch::Refresh(observed_generation) => {
                if let Some(entry) = current
                    && entry.generation != observed_generation
                {
                    flight.ensure_needed()?;
                    return Ok(entry.stream);
                }
            }
        }

        let spec = self.command(item, auth)?;
        let process_result = tokio::select! {
            biased;
            () = flight.cancel.cancelled() => return Err(cancelled()),
            result = self.runner.output(spec) => result,
        };
        flight.ensure_needed()?;
        let output = process_result.map_err(|error| process_error(&error))?;
        flight.ensure_needed()?;
        if !output.status.success() {
            flight.ensure_needed()?;
            return Err(extractor_error(&output, auth));
        }

        flight.ensure_needed()?;
        let resolved_at = self.clock.now();
        flight.ensure_needed()?;
        let stream = parse_stream(item, &output.stdout, resolved_at)?;
        flight.ensure_needed()?;
        let publish_time = self.clock.now();
        flight.ensure_needed()?;
        self.cache_success(key, stream.clone(), publish_time, &flight.cancel)?;
        Ok(stream)
    }

    fn command(
        &self,
        item: &MediaItem,
        auth: Option<&CookieFile>,
    ) -> Result<CommandSpec, ResolveError> {
        let mut watch_url = Url::parse("https://music.youtube.com/watch").map_err(|_| {
            ResolveError::new(
                ResolveErrorCategory::InvalidInput,
                "could not construct a YouTube Music watch URL",
            )
        })?;
        watch_url
            .query_pairs_mut()
            .append_pair("v", &item.id.video_id);

        let mut args = vec![
            OsString::from("--ignore-config"),
            OsString::from("-J"),
            OsString::from("--no-playlist"),
            OsString::from("--no-warnings"),
            OsString::from("--format"),
            OsString::from("ba/b"),
        ];
        if let Some(cookie) = auth {
            args.push(OsString::from("--cookies"));
            args.push(cookie.path().as_os_str().to_os_string());
        }
        args.push(OsString::from("--"));
        args.push(OsString::from(watch_url.as_str()));

        Ok(
            CommandSpec::new(&self.program, args).with_limits(ProcessLimits {
                timeout: RESOLVE_TIMEOUT,
                max_stdout_bytes: MAX_STDOUT_BYTES,
                max_stderr_bytes: MAX_STDERR_BYTES,
            }),
        )
    }
}

#[async_trait]
impl Resolver for YtDlpResolver {
    async fn resolve_with_policy(
        &self,
        item: &MediaItem,
        auth: Option<&CookieFile>,
        policy: ResolvePolicy,
        cancel: CancellationToken,
    ) -> Result<ResolvedStream, ResolveError> {
        active(&cancel)?;
        validate_item(item)?;

        let key = Self::cache_key(item, auth);
        let initial = self.cache_entry(&key, self.clock.now());
        active(&cancel)?;
        let observed_generation = initial.as_ref().map(|entry| entry.generation);
        if policy == ResolvePolicy::UseCache
            && let Some(entry) = initial
        {
            active(&cancel)?;
            return Ok(entry.stream);
        }

        let epoch = match (policy, observed_generation) {
            (ResolvePolicy::UseCache, _) | (ResolvePolicy::ForceRefresh, None) => {
                FlightEpoch::Missing
            }
            (ResolvePolicy::ForceRefresh, Some(generation)) => FlightEpoch::Refresh(generation),
        };
        let flight_key = FlightKey { cache: key, epoch };
        let (waiter, start) = self.join_flight(&flight_key, &cancel);
        if start {
            self.start_flight(&flight_key, &waiter.flight, item.clone(), auth.cloned());
        }
        waiter.wait(cancel).await
    }
}

fn validate_item(item: &MediaItem) -> Result<(), ResolveError> {
    if item.id.provider != SUPPORTED_PROVIDER {
        return Err(ResolveError::new(
            ResolveErrorCategory::UnsupportedInput,
            "only YouTube Music media can be resolved",
        ));
    }
    if item.id.video_id.is_empty() {
        return Err(ResolveError::new(
            ResolveErrorCategory::InvalidInput,
            "the media video ID is empty",
        ));
    }
    Ok(())
}

fn cancelled() -> ResolveError {
    ResolveError::new(
        ResolveErrorCategory::Cancellation,
        "stream resolution was cancelled",
    )
}

fn active(cancel: &CancellationToken) -> Result<(), ResolveError> {
    if cancel.is_cancelled() {
        Err(cancelled())
    } else {
        Ok(())
    }
}

fn process_error(error: &ProcessError) -> ResolveError {
    let message = match error {
        ProcessError::Spawn { .. } => "could not start the media resolver dependency",
        ProcessError::Wait { .. } => "could not wait for the media resolver process",
        ProcessError::Read { .. } => "could not read captured media resolver output",
        ProcessError::CaptureUnavailable { .. } => {
            "the media resolver did not expose captured output"
        }
        ProcessError::Timeout { .. } => "the media resolver process timed out",
        ProcessError::OutputLimitExceeded { .. } => {
            "the media resolver exceeded its output capture limit"
        }
        ProcessError::Terminate { .. } => "could not terminate the media resolver process",
    };
    ResolveError::new(ResolveErrorCategory::Process, message)
}

fn extractor_error(output: &ProcessOutput, auth: Option<&CookieFile>) -> ResolveError {
    let status = output
        .status
        .code()
        .map_or_else(|| "signal".to_owned(), |code| format!("code {code}"));
    let stderr = sanitized_stderr(&output.stderr, auth);
    let message = if stderr.is_empty() {
        format!("yt-dlp exited with {status}")
    } else {
        format!("yt-dlp exited with {status}: {stderr}")
    };
    ResolveError::new(ResolveErrorCategory::Extractor, message)
}

fn sanitized_stderr(stderr: &[u8], auth: Option<&CookieFile>) -> String {
    let bounded = stderr.get(..MAX_STDERR_BYTES).unwrap_or(stderr);
    let mut rendered = String::from_utf8_lossy(bounded).into_owned();
    if let Some(cookie) = auth {
        let path = cookie.path().to_string_lossy();
        if !path.is_empty() {
            rendered = rendered.replace(path.as_ref(), "[REDACTED]");
        }
    }
    compact(sanitize(&rendered).trim(), MAX_ERROR_CHARS)
}

fn compact(value: &str, max_chars: usize) -> String {
    let mut characters = value.chars();
    let compact = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{compact}…")
    } else {
        compact
    }
}

#[derive(Deserialize)]
struct ExtractorResponse {
    url: Option<String>,
    title: Option<String>,
    duration: Option<f64>,
    acodec: Option<String>,
    format_id: Option<String>,
    formats: Option<serde_json::Value>,
}

fn parse_stream(
    item: &MediaItem,
    stdout: &[u8],
    resolved_at: OffsetDateTime,
) -> Result<ResolvedStream, ResolveError> {
    let response = serde_json::from_slice::<ExtractorResponse>(stdout).map_err(|_| {
        ResolveError::new(
            ResolveErrorCategory::InvalidResponse,
            "yt-dlp returned malformed JSON",
        )
    })?;
    let raw_url = response.url.ok_or_else(|| {
        ResolveError::new(
            ResolveErrorCategory::MissingStream,
            "yt-dlp did not return a stream URL",
        )
    })?;
    let mut stream = ResolvedStream::from_raw_audio_url(item.id.clone(), &raw_url, resolved_at)
        .map_err(|error| match error {
            ResolvedAudioUrlError::Empty => ResolveError::new(
                ResolveErrorCategory::MissingStream,
                "yt-dlp returned an empty stream URL",
            ),
            ResolvedAudioUrlError::Invalid => ResolveError::new(
                ResolveErrorCategory::InvalidResponse,
                "yt-dlp returned an invalid stream URL",
            ),
            ResolvedAudioUrlError::UnsupportedScheme => ResolveError::new(
                ResolveErrorCategory::InvalidResponse,
                "yt-dlp returned an unsupported stream URL scheme",
            ),
            ResolvedAudioUrlError::MissingHost => ResolveError::new(
                ResolveErrorCategory::InvalidResponse,
                "yt-dlp returned a stream URL without a host",
            ),
        })?;

    let preview_url = (item.kind == MediaKind::Video)
        .then(|| select_preview(response.formats.as_ref()))
        .flatten();

    stream.preview_url = preview_url;
    stream.title = response.title;
    stream.duration_ms = duration_ms(response.duration)?;
    stream.codec = response.acodec;
    stream.format_id = response.format_id;
    Ok(stream)
}

fn select_preview(formats: Option<&serde_json::Value>) -> Option<PreviewStreamUrl> {
    formats?
        .as_array()?
        .iter()
        .take(MAX_PREVIEW_FORMATS)
        .filter_map(preview_candidate)
        .max_by_key(|candidate| candidate.score)
        .map(|candidate| candidate.url)
}

struct PreviewCandidate {
    url: PreviewStreamUrl,
    score: (u64, u64, u64),
}

fn preview_candidate(value: &serde_json::Value) -> Option<PreviewCandidate> {
    let object = value.as_object()?;
    if object.get("acodec")?.as_str()? != "none" {
        return None;
    }
    let video_codec = object.get("vcodec")?.as_str()?;
    if video_codec.is_empty() || video_codec == "none" {
        return None;
    }
    let width = object.get("width")?.as_u64()?;
    let height = object.get("height")?.as_u64()?;
    let fps = object.get("fps")?.as_f64()?;
    if width == 0
        || width > MAX_PREVIEW_WIDTH
        || height == 0
        || height > MAX_PREVIEW_HEIGHT
        || !fps.is_finite()
        || fps <= 0.0
        || fps > MAX_PREVIEW_FPS
    {
        return None;
    }
    let url = PreviewStreamUrl::parse(object.get("url")?.as_str()?).ok()?;
    Some(PreviewCandidate {
        url,
        score: (width.saturating_mul(height), width, height),
    })
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite nonnegative milliseconds are range-checked before conversion"
)]
fn duration_ms(seconds: Option<f64>) -> Result<Option<u64>, ResolveError> {
    let Some(seconds) = seconds else {
        return Ok(None);
    };
    if !seconds.is_finite() || seconds.is_sign_negative() {
        return Err(invalid_duration());
    }
    let milliseconds = seconds * 1_000.0;
    if !milliseconds.is_finite() {
        return Err(invalid_duration());
    }
    let rounded = milliseconds.round();
    if rounded >= FIRST_INVALID_U64 {
        return Err(invalid_duration());
    }
    Ok(Some(rounded as u64))
}

fn invalid_duration() -> ResolveError {
    ResolveError::new(
        ResolveErrorCategory::InvalidResponse,
        "yt-dlp returned an invalid stream duration",
    )
}

fn is_live(now: OffsetDateTime, resolved_at: OffsetDateTime, ttl: Duration) -> bool {
    if ttl.is_zero() {
        return false;
    }
    let elapsed = now - resolved_at;
    if elapsed.is_negative() {
        return false;
    }
    time::Duration::try_from(ttl).is_ok_and(|ttl| elapsed < ttl)
}
