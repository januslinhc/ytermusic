use std::{
    collections::VecDeque,
    ffi::OsString,
    io,
    path::PathBuf,
    process::ExitStatus,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use time::{OffsetDateTime, macros::datetime};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use ytermusic::{
    domain::{MediaId, MediaItem, MediaKind},
    process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner},
    resolver::{
        AnalysisStreamUrl, AuthIdentity, CookieFile, PreviewStreamUrl, ResolveErrorCategory,
        ResolvePolicy, ResolvedStream, Resolver, ResolverClock, YtDlpResolver,
    },
};

const FIXTURE: &[u8] = include_bytes!("fixtures/ytdlp_song.json");

#[cfg(unix)]
fn exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;

    ExitStatus::from_raw(code << 8)
}

#[cfg(windows)]
fn exit_status(code: i32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt as _;

    #[expect(clippy::cast_sign_loss, reason = "test exit codes are non-negative")]
    ExitStatus::from_raw(code as u32)
}

#[cfg(unix)]
fn signal_status(signal: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt as _;

    ExitStatus::from_raw(signal)
}

fn output(code: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> ProcessOutput {
    ProcessOutput {
        status: exit_status(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

fn fixture_output() -> ProcessOutput {
    output(0, FIXTURE, Vec::new())
}

fn stream_output(url: &str) -> ProcessOutput {
    let value = serde_json::json!({
        "url": url,
        "title": "Invented track",
        "duration": 4.25,
        "acodec": "opus",
        "format_id": "251"
    });
    output(0, value.to_string(), Vec::new())
}

fn item(video_id: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind: MediaKind::Song,
        title: "Invented track".to_owned(),
        creators: vec!["Fixture Artist".to_owned()],
        collection: None,
        duration_ms: None,
        artwork_url: None,
        explicit: false,
    }
}

fn video(video_id: &str) -> MediaItem {
    MediaItem {
        kind: MediaKind::Video,
        ..item(video_id)
    }
}

fn fixed_time() -> OffsetDateTime {
    datetime!(2026-07-24 12:00 UTC)
}

fn identity(value: &str) -> AuthIdentity {
    match AuthIdentity::new(value) {
        Ok(identity) => identity,
        Err(error) => panic!("test identity should be valid: {error}"),
    }
}

fn parsed_url(value: &str) -> url::Url {
    match url::Url::parse(value) {
        Ok(url) => url,
        Err(error) => panic!("test URL should parse: {error}"),
    }
}

fn called_video_id(spec: &CommandSpec) -> Option<String> {
    let argument = spec.args.last()?.to_str()?;
    let url = url::Url::parse(argument).ok()?;
    url.query_pairs()
        .find(|(key, _)| key == "v")
        .map(|(_, value)| value.into_owned())
}

enum FakeResponse {
    Output(ProcessOutput),
    Error(ProcessError),
    Pending(Arc<Notify>),
    Blocked {
        started: Arc<Notify>,
        release: Arc<Notify>,
        output: ProcessOutput,
    },
    CancelThenOutput {
        cancel: CancellationToken,
        output: ProcessOutput,
    },
}

struct FakeRunner {
    responses: Mutex<VecDeque<FakeResponse>>,
    calls: Mutex<Vec<CommandSpec>>,
}

impl FakeRunner {
    fn new(responses: impl IntoIterator<Item = FakeResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<CommandSpec> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn call_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

#[async_trait]
impl ProcessRunner for FakeRunner {
    async fn output(&self, spec: CommandSpec) -> Result<ProcessOutput, ProcessError> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(spec.clone());

        let response = self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected process call: {spec:?}"));
        match response {
            FakeResponse::Output(output) => Ok(output),
            FakeResponse::Error(error) => Err(error),
            FakeResponse::Pending(started) => {
                started.notify_one();
                std::future::pending().await
            }
            FakeResponse::Blocked {
                started,
                release,
                output,
            } => {
                started.notify_one();
                release.notified().await;
                Ok(output)
            }
            FakeResponse::CancelThenOutput { cancel, output } => {
                cancel.cancel();
                Ok(output)
            }
        }
    }
}

struct FakeClock {
    now: Mutex<OffsetDateTime>,
    reads: AtomicUsize,
    read: Notify,
}

impl FakeClock {
    fn new(now: OffsetDateTime) -> Self {
        Self {
            now: Mutex::new(now),
            reads: AtomicUsize::new(0),
            read: Notify::new(),
        }
    }

    fn set(&self, now: OffsetDateTime) {
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
    }

    fn read_count(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    async fn wait_for_reads(&self, expected: usize) {
        loop {
            let read = self.read.notified();
            if self.read_count() >= expected {
                return;
            }
            read.await;
        }
    }
}

impl ResolverClock for FakeClock {
    fn now(&self) -> OffsetDateTime {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.read.notify_waiters();
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[tokio::test]
async fn command_uses_exact_direct_argv_and_url_encodes_the_video_id() {
    let runner = Arc::new(FakeRunner::new([FakeResponse::Output(fixture_output())]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = YtDlpResolver::new(
        PathBuf::from("/opt/tools/yt-dlp"),
        runner.clone(),
        clock,
        Duration::ZERO,
    );
    let weird_id = "invented &?=/% --cookies";

    let result = resolver
        .resolve(&item(weird_id), None, CancellationToken::new())
        .await;
    assert!(result.is_ok(), "fixture resolution should succeed");

    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.program, PathBuf::from("/opt/tools/yt-dlp"));
    assert_eq!(call.limits.timeout, Duration::from_secs(30));
    assert_eq!(call.limits.max_stdout_bytes, 4 * 1_024 * 1_024);
    assert_eq!(call.limits.max_stderr_bytes, 16 * 1_024);
    assert_eq!(
        call.args,
        vec![
            OsString::from("--ignore-config"),
            OsString::from("-J"),
            OsString::from("--no-playlist"),
            OsString::from("--no-warnings"),
            OsString::from("--format"),
            OsString::from("ba/b"),
            OsString::from("--"),
            OsString::from("https://music.youtube.com/watch?v=invented+%26%3F%3D%2F%25+--cookies"),
        ]
    );

    let watch_url = call.args.last().map(|value| value.to_string_lossy());
    let watch_url = match watch_url {
        Some(watch_url) => parsed_url(&watch_url),
        None => panic!("watch URL argument should be present"),
    };
    let query = watch_url.query_pairs().collect::<Vec<_>>();
    assert_eq!(query, vec![("v".into(), weird_id.into())]);
}

#[tokio::test]
async fn authenticated_command_keeps_cookie_path_as_one_os_argument_and_secrets_opaque() {
    let runner = Arc::new(FakeRunner::new([FakeResponse::Output(fixture_output())]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );
    let cookie_path = PathBuf::from("/tmp/private cookies/--session.txt");
    let auth = CookieFile::new(cookie_path.clone(), identity("vault-account-7"));

    let result = resolver
        .resolve(&item("fixture-auth"), Some(&auth), CancellationToken::new())
        .await;
    assert!(result.is_ok(), "authenticated resolution should succeed");

    let calls = runner.calls();
    let args = &calls[0].args;
    assert_eq!(
        args,
        &[
            OsString::from("--ignore-config"),
            OsString::from("-J"),
            OsString::from("--no-playlist"),
            OsString::from("--no-warnings"),
            OsString::from("--format"),
            OsString::from("ba/b"),
            OsString::from("--cookies"),
            cookie_path.as_os_str().to_os_string(),
            OsString::from("--"),
            OsString::from("https://music.youtube.com/watch?v=fixture-auth"),
        ]
    );

    let cookie_debug = format!("{auth:?}");
    let cookie_display = auth.to_string();
    let identity = identity("vault-account-7");
    let identity_debug = format!("{identity:?}");
    let identity_display = identity.to_string();
    for rendered in [
        cookie_debug,
        cookie_display,
        identity_debug,
        identity_display,
    ] {
        assert!(!rendered.contains("private cookies"));
        assert!(!rendered.contains("session.txt"));
        assert!(!rendered.contains("vault-account-7"));
    }

    let empty_identity = AuthIdentity::new("");
    assert!(
        empty_identity.is_err(),
        "empty cache identities must be rejected"
    );
}

#[tokio::test]
async fn fixture_maps_to_a_typed_resolved_stream() {
    let runner = Arc::new(FakeRunner::new([FakeResponse::Output(fixture_output())]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );
    let media = item("fixture-song-001");

    let stream = match resolver
        .resolve(&media, None, CancellationToken::new())
        .await
    {
        Ok(stream) => stream,
        Err(error) => panic!("fixture should map successfully: {error}"),
    };

    let mut expected = ResolvedStream::new(
        media.id,
        parsed_url("http://127.0.0.1:41000/audio?fixture=temporary#segment"),
        fixed_time(),
    );
    expected.title = Some("Harbor Lights (Fixture)".to_owned());
    expected.duration_ms = Some(183_125);
    expected.codec = Some("opus".to_owned());
    expected.format_id = Some("251".to_owned());
    assert_eq!(stream, expected);
}

#[test]
fn resolved_stream_debug_redacts_url_userinfo_query_and_fragment() {
    let preview = match PreviewStreamUrl::parse(
        "https://preview-user:preview-password@video.invalid/private-preview?token=preview-secret#frame",
    ) {
        Ok(preview) => preview,
        Err(error) => panic!("test preview URL should be valid: {error}"),
    };
    let mut stream = ResolvedStream::new(
        MediaId {
            provider: "provider-debug-secret".to_owned(),
            video_id: "video-debug-secret".to_owned(),
        },
        parsed_url(
            "https://fixture-user:fixture-password@127.0.0.1/audio/fixture-path-secret?signature=fixture-query-secret#private",
        ),
        fixed_time(),
    );
    stream.preview_url = Some(preview.clone());
    stream.title = Some("title-debug-secret".to_owned());
    stream.duration_ms = Some(1_000);
    stream.codec = Some("opus".to_owned());
    stream.format_id = Some("251".to_owned());

    let debug = format!("{stream:?}");
    assert!(debug.contains("ResolvedStream"));
    assert!(debug.contains("[REDACTED stream URL]"));
    assert!(debug.contains("[REDACTED preview stream URL]"));
    assert!(!debug.contains("fixture-user"));
    assert!(!debug.contains("fixture-password"));
    assert!(!debug.contains("127.0.0.1"));
    assert!(!debug.contains("fixture-path-secret"));
    assert!(!debug.contains("fixture-query-secret"));
    assert!(!debug.contains("private"));
    assert!(!debug.contains("preview-user"));
    assert!(!debug.contains("preview-password"));
    assert!(!debug.contains("private-preview"));
    assert!(!debug.contains("preview-secret"));
    assert!(!debug.contains("provider-debug-secret"));
    assert!(!debug.contains("video-debug-secret"));
    assert!(!debug.contains("title-debug-secret"));
    assert!(debug.contains("REDACTED provider identity"));
    assert!(debug.contains("duration_ms"));
    assert!(debug.contains("codec"));
    assert!(debug.contains("format_id"));

    for rendered in [format!("{preview:?}"), preview.to_string()] {
        assert!(rendered.contains("REDACTED"));
        assert!(!rendered.contains("video.invalid"));
        assert!(!rendered.contains("preview-secret"));
    }
}

#[tokio::test]
async fn preview_selects_deterministic_bounded_video_only_format_for_video_media() {
    let runner = Arc::new(FakeRunner::new([FakeResponse::Output(fixture_output())]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );

    let stream = match resolver
        .resolve(&video("fixture-video-001"), None, CancellationToken::new())
        .await
    {
        Ok(stream) => stream,
        Err(error) => panic!("video fixture should preserve successful audio: {error}"),
    };

    assert_eq!(
        stream.preview_url.as_ref().map(PreviewStreamUrl::as_url),
        Some(&parsed_url(
            "https://video.invalid/360?signature=fixture-selected"
        ))
    );
    assert_eq!(
        stream.url,
        parsed_url("http://127.0.0.1:41000/audio?fixture=temporary#segment")
    );
    assert_eq!(stream.codec.as_deref(), Some("opus"));
    assert_eq!(stream.format_id.as_deref(), Some("251"));
}

#[tokio::test]
async fn preview_is_absent_for_songs_and_invalid_formats_never_fail_audio() {
    let invalid_formats = serde_json::json!({
        "url": "https://media.invalid/audio",
        "acodec": "opus",
        "format_id": "251",
        "formats": [
            {
                "url": "http://video.invalid/insecure",
                "acodec": "none",
                "vcodec": "avc1",
                "width": 426,
                "height": 240,
                "fps": 30
            },
            {
                "url": "https://video.invalid/no-dimensions",
                "acodec": "none",
                "vcodec": "avc1"
            },
            {
                "url": "https://video.invalid/high-fps",
                "acodec": "none",
                "vcodec": "avc1",
                "width": 426,
                "height": 240,
                "fps": 60
            }
        ]
    });
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(fixture_output()),
        FakeResponse::Output(output(0, invalid_formats.to_string(), Vec::new())),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );

    let song = resolver
        .resolve(&item("fixture-song"), None, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("song audio should resolve: {error}"));
    assert!(song.preview_url.is_none());

    let video_stream = resolver
        .resolve(&video("invalid-preview"), None, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("invalid preview must not fail audio: {error}"));
    assert!(video_stream.preview_url.is_none());
    assert_eq!(video_stream.url, parsed_url("https://media.invalid/audio"));
}

#[tokio::test]
async fn preview_cache_identity_keeps_song_and_video_eligibility_separate() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(fixture_output()),
        FakeResponse::Output(fixture_output()),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    );

    let song = resolver
        .resolve(&item("shared-kind"), None, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("song should resolve: {error}"));
    let video_stream = resolver
        .resolve(&video("shared-kind"), None, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("video should resolve independently: {error}"));

    assert!(song.preview_url.is_none());
    assert!(video_stream.preview_url.is_some());
    assert_eq!(runner.call_count(), 2);

    let reverse_runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(fixture_output()),
        FakeResponse::Output(fixture_output()),
    ]));
    let reverse_resolver = YtDlpResolver::new(
        "yt-dlp",
        reverse_runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    );
    let video_stream = reverse_resolver
        .resolve(&video("reverse-kind"), None, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("video should resolve: {error}"));
    let song = reverse_resolver
        .resolve(&item("reverse-kind"), None, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("song should resolve independently: {error}"));

    assert!(video_stream.preview_url.is_some());
    assert!(song.preview_url.is_none());
    assert_eq!(reverse_runner.call_count(), 2);
}

#[test]
fn preview_url_rejects_non_https_missing_hosts_and_oversized_values() {
    assert!(PreviewStreamUrl::parse("http://video.invalid/preview").is_err());
    assert!(PreviewStreamUrl::parse("https://").is_err());
    let oversized = format!("https://video.invalid/{}", "x".repeat(8_192));
    assert!(PreviewStreamUrl::parse(&oversized).is_err());
    let expands_when_encoded = format!("https://video.invalid/{}", "é".repeat(4_000));
    assert!(PreviewStreamUrl::parse(&expands_when_encoded).is_err());
}

#[test]
fn analysis_stream_url_is_bounded_https_and_redacted() {
    let make_stream = |value: &str| {
        let mut stream = ResolvedStream::new(
            MediaId {
                provider: "analysis-provider-secret".to_owned(),
                video_id: "analysis-video-secret".to_owned(),
            },
            parsed_url(value),
            fixed_time(),
        );
        stream.title = Some("analysis-title-secret".to_owned());
        stream.duration_ms = Some(1_000);
        stream.codec = Some("opus".to_owned());
        stream.format_id = Some("251".to_owned());
        stream
    };

    let stream = make_stream(
        "https://analysis-user:analysis-password@media.invalid/audio?token=analysis-secret#fragment",
    );
    let analysis: AnalysisStreamUrl = stream
        .analysis_stream_url()
        .unwrap_or_else(|| panic!("bounded HTTPS audio URL should be eligible for analysis"));
    assert_eq!(
        analysis.as_url().as_str(),
        "https://analysis-user:analysis-password@media.invalid/audio?token=analysis-secret#fragment"
    );
    for rendered in [format!("{analysis:?}"), analysis.to_string()] {
        assert!(rendered.contains("REDACTED"));
        for sentinel in [
            "analysis-user",
            "analysis-password",
            "media.invalid",
            "analysis-secret",
            "analysis-provider-secret",
            "analysis-video-secret",
            "analysis-title-secret",
        ] {
            assert!(!rendered.contains(sentinel));
        }
    }

    assert!(
        make_stream("http://media.invalid/audio")
            .analysis_stream_url()
            .is_none()
    );
    assert!(
        make_stream("file:///private/audio")
            .analysis_stream_url()
            .is_none()
    );
    let oversized_before_parse = format!("https://media.invalid/{}", "x".repeat(8_192));
    assert!(
        make_stream(&oversized_before_parse)
            .analysis_stream_url()
            .is_none()
    );
    let oversized_after_parse = format!("https://media.invalid/{}", "é".repeat(4_000));
    assert!(
        make_stream(&oversized_after_parse)
            .analysis_stream_url()
            .is_none()
    );
}

#[test]
fn analysis_stream_url_never_diverges_from_mutated_playback_url() {
    let media_id = MediaId {
        provider: "youtube-music".to_owned(),
        video_id: "mutable-analysis".to_owned(),
    };
    let mut stream = ResolvedStream::from_raw_audio_url(
        media_id.clone(),
        "https://media.invalid/original?token=old-secret",
        fixed_time(),
    )
    .unwrap_or_else(|error| panic!("original audio URL should parse: {error}"));
    stream.url = parsed_url("https://media.invalid/replacement?token=new-secret");
    assert_eq!(
        stream
            .analysis_stream_url()
            .as_ref()
            .map(AnalysisStreamUrl::as_url),
        Some(&stream.url)
    );
    stream.url = parsed_url("http://media.invalid/playback-only");
    assert!(stream.analysis_stream_url().is_none());

    let oversized_raw = format!("https://media.invalid/{}audio", "segment/../".repeat(800));
    let mut conservatively_ineligible =
        ResolvedStream::from_raw_audio_url(media_id, &oversized_raw, fixed_time())
            .unwrap_or_else(|error| panic!("oversized raw audio URL should still play: {error}"));
    conservatively_ineligible.url = parsed_url("https://media.invalid/short-replacement");
    assert!(conservatively_ineligible.analysis_stream_url().is_none());
}

#[tokio::test]
async fn oversized_raw_analysis_url_keeps_audio_but_disables_analysis() {
    let raw_url = format!(
        "https://media.invalid/{}audio?token=raw-analysis-secret",
        "segment/../".repeat(800)
    );
    assert!(raw_url.len() > 8_192);
    assert!(
        parsed_url(&raw_url).as_str().len() < 8_192,
        "the regression requires URL parsing to hide the oversized raw input"
    );
    let runner = Arc::new(FakeRunner::new([FakeResponse::Output(stream_output(
        &raw_url,
    ))]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );

    let stream = resolver
        .resolve(
            &item("oversized-raw-analysis"),
            None,
            CancellationToken::new(),
        )
        .await;
    let stream = stream.unwrap_or_else(|error| {
        panic!("oversized analysis input must not block valid audio: {error}")
    });

    assert_eq!(stream.url, parsed_url(&raw_url));
    assert!(stream.analysis_stream_url().is_none());
    let debug = format!("{stream:?}");
    assert!(!debug.contains("raw-analysis-secret"));
    assert!(!debug.contains("media.invalid"));
}

#[tokio::test]
async fn nonzero_exit_is_typed_and_exposes_only_bounded_sanitized_stderr() {
    let cookie_path = PathBuf::from("/tmp/top secret/cookies.txt");
    let stderr = format!(
        "Cookie: fixture-cookie-value {} https://user:password@127.0.0.1/audio?signature=secret#frag {}",
        cookie_path.to_string_lossy(),
        "tail".repeat(5_000)
    );
    let runner = Arc::new(FakeRunner::new([FakeResponse::Output(output(
        7,
        b"https://127.0.0.1/?stdout-secret=yes".to_vec(),
        stderr,
    ))]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );
    let auth = CookieFile::new(cookie_path.clone(), identity("private-identity"));

    let Err(error) = resolver
        .resolve(
            &item("extractor-failure"),
            Some(&auth),
            CancellationToken::new(),
        )
        .await
    else {
        panic!("nonzero extractor status must fail");
    };

    assert_eq!(error.category(), ResolveErrorCategory::Extractor);
    let display = error.to_string();
    let debug = format!("{error:?}");
    for rendered in [&display, &debug] {
        assert!(rendered.contains("code 7"));
        assert!(rendered.len() < 1_024, "diagnostic must remain compact");
        assert!(!rendered.contains("fixture-cookie-value"));
        assert!(!rendered.contains("top secret"));
        assert!(!rendered.contains("cookies.txt"));
        assert!(!rendered.contains("user"));
        assert!(!rendered.contains("password"));
        assert!(!rendered.contains("signature=secret"));
        assert!(!rendered.contains("stdout-secret"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn signal_termination_is_reported_without_raw_output() {
    let runner = Arc::new(FakeRunner::new([FakeResponse::Output(ProcessOutput {
        status: signal_status(9),
        stdout: Vec::new(),
        stderr: b"https://127.0.0.1/audio?token=do-not-print".to_vec(),
    })]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );

    let Err(error) = resolver
        .resolve(&item("signal-failure"), None, CancellationToken::new())
        .await
    else {
        panic!("signal termination must fail");
    };

    assert_eq!(error.category(), ResolveErrorCategory::Extractor);
    assert!(error.to_string().contains("signal"));
    assert!(!error.to_string().contains("do-not-print"));
}

#[tokio::test]
async fn missing_empty_malformed_and_invalid_stream_fields_are_typed() {
    let responses = [
        FakeResponse::Output(output(0, b"{}".to_vec(), Vec::new())),
        FakeResponse::Output(output(0, br#"{"url":"   "}"#.to_vec(), Vec::new())),
        FakeResponse::Output(output(
            0,
            b"{not valid JSON https://127.0.0.1/?secret=yes".to_vec(),
            Vec::new(),
        )),
        FakeResponse::Output(output(
            0,
            br#"{"url":"file:///tmp/audio","duration":1}"#.to_vec(),
            Vec::new(),
        )),
        FakeResponse::Output(output(
            0,
            br#"{"url":"https://","duration":1}"#.to_vec(),
            Vec::new(),
        )),
        FakeResponse::Output(output(
            0,
            br#"{"url":"http://127.0.0.1/audio","duration":-0.01}"#.to_vec(),
            Vec::new(),
        )),
        FakeResponse::Output(output(
            0,
            br#"{"url":"http://127.0.0.1/audio","duration":1e300}"#.to_vec(),
            Vec::new(),
        )),
    ];
    let runner = Arc::new(FakeRunner::new(responses));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );
    let expected = [
        ResolveErrorCategory::MissingStream,
        ResolveErrorCategory::MissingStream,
        ResolveErrorCategory::InvalidResponse,
        ResolveErrorCategory::InvalidResponse,
        ResolveErrorCategory::InvalidResponse,
        ResolveErrorCategory::InvalidResponse,
        ResolveErrorCategory::InvalidResponse,
    ];

    for (index, category) in expected.into_iter().enumerate() {
        let Err(error) = resolver
            .resolve(
                &item(&format!("invalid-{index}")),
                None,
                CancellationToken::new(),
            )
            .await
        else {
            panic!("invalid response {index} must fail");
        };
        assert_eq!(error.category(), category);
        let rendered = format!("{error:?}");
        assert!(!rendered.contains("secret=yes"));
        assert!(!rendered.contains("{not valid"));
    }
}

#[tokio::test]
async fn unsupported_provider_and_empty_video_id_never_start_the_runner() {
    let runner = Arc::new(FakeRunner::new([]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    );
    let mut unsupported = item("fixture-id");
    unsupported.id.provider = "other-provider".to_owned();

    let Err(unsupported_error) = resolver
        .resolve(&unsupported, None, CancellationToken::new())
        .await
    else {
        panic!("unsupported provider must fail");
    };
    assert_eq!(
        unsupported_error.category(),
        ResolveErrorCategory::UnsupportedInput
    );

    let Err(empty_error) = resolver
        .resolve(&item(""), None, CancellationToken::new())
        .await
    else {
        panic!("empty video ID must fail");
    };
    assert_eq!(empty_error.category(), ResolveErrorCategory::InvalidInput);
    assert_eq!(runner.call_count(), 0);
}

#[tokio::test]
async fn live_cache_hits_and_exact_expiry_reruns_the_extractor() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/first")),
        FakeResponse::Output(stream_output("http://127.0.0.1/refreshed")),
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_secs(10),
    );
    let media = item("cache-expiry");

    let first = match resolver
        .resolve(&media, None, CancellationToken::new())
        .await
    {
        Ok(stream) => stream,
        Err(error) => panic!("first resolution should succeed: {error}"),
    };
    clock.set(fixed_time() + time::Duration::seconds(9));
    let cached = match resolver
        .resolve(&media, None, CancellationToken::new())
        .await
    {
        Ok(stream) => stream,
        Err(error) => panic!("live cache lookup should succeed: {error}"),
    };
    assert_eq!(cached, first);
    assert_eq!(runner.call_count(), 1);

    clock.set(fixed_time() + time::Duration::seconds(10));
    let refreshed = match resolver
        .resolve(&media, None, CancellationToken::new())
        .await
    {
        Ok(stream) => stream,
        Err(error) => panic!("expired cache should refresh: {error}"),
    };
    assert_eq!(refreshed.url, parsed_url("http://127.0.0.1/refreshed"));
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn force_refresh_bypasses_and_replaces_the_observed_cached_stream() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/old")),
        FakeResponse::Output(stream_output("http://127.0.0.1/fresh")),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    );
    let media = item("force-refresh");

    let first = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    let ordinary = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    let refreshed = resolver
        .resolve_with_policy(
            &media,
            None,
            ResolvePolicy::ForceRefresh,
            CancellationToken::new(),
        )
        .await;
    let after_refresh = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;

    assert_eq!(
        first.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/old"))
    );
    assert_eq!(ordinary, first);
    assert_eq!(
        refreshed.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/fresh"))
    );
    assert_eq!(after_refresh, refreshed);
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn concurrent_force_refreshes_of_one_generation_coalesce() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/old")),
        FakeResponse::Blocked {
            started: started.clone(),
            release: release.clone(),
            output: stream_output("http://127.0.0.1/fresh"),
        },
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = Arc::new(YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_mins(1),
    ));
    let media = item("concurrent-force-refresh");
    let primed = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    assert!(primed.is_ok());
    let reads_before_refresh = clock.read_count();

    let first_resolver = resolver.clone();
    let first_media = media.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve_with_policy(
                &first_media,
                None,
                ResolvePolicy::ForceRefresh,
                CancellationToken::new(),
            )
            .await
    });
    let wait_for_runner = started.notified();
    let runner_started = tokio::time::timeout(Duration::from_secs(1), wait_for_runner).await;
    assert!(runner_started.is_ok(), "first refresh should start runner");

    let second_resolver = resolver.clone();
    let second_media = media.clone();
    let second = tokio::spawn(async move {
        second_resolver
            .resolve_with_policy(
                &second_media,
                None,
                ResolvePolicy::ForceRefresh,
                CancellationToken::new(),
            )
            .await
    });
    let both_observed = tokio::time::timeout(
        Duration::from_secs(1),
        clock.wait_for_reads(reads_before_refresh + 3),
    )
    .await;
    assert!(
        both_observed.is_ok(),
        "both refreshes should observe the old generation"
    );
    release.notify_one();

    let first = match first.await {
        Ok(result) => result,
        Err(error) => panic!("first refresh task should not panic: {error}"),
    };
    let second = match second.await {
        Ok(result) => result,
        Err(error) => panic!("second refresh task should not panic: {error}"),
    };
    assert_eq!(first, second);
    assert_eq!(
        first.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/fresh"))
    );
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn missing_after_eviction_joins_the_active_refresh_for_the_same_key() {
    let refresh_started = Arc::new(Notify::new());
    let release_refresh = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/a-old")),
        FakeResponse::Blocked {
            started: refresh_started.clone(),
            release: release_refresh.clone(),
            output: stream_output("http://127.0.0.1/a-refreshed"),
        },
        FakeResponse::Output(stream_output("http://127.0.0.1/b")),
        FakeResponse::Output(stream_output("http://127.0.0.1/a-duplicate")),
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = Arc::new(YtDlpResolver::with_cache_capacity(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_mins(1),
        1,
    ));
    let media_a = item("compatible-refresh-a");

    let primed = resolver
        .resolve(&media_a, None, CancellationToken::new())
        .await;
    assert!(primed.is_ok());

    let refresh_resolver = resolver.clone();
    let refresh_media = media_a.clone();
    let refresh = tokio::spawn(async move {
        refresh_resolver
            .resolve_with_policy(
                &refresh_media,
                None,
                ResolvePolicy::ForceRefresh,
                CancellationToken::new(),
            )
            .await
    });
    let refresh_running =
        tokio::time::timeout(Duration::from_secs(1), refresh_started.notified()).await;
    assert!(refresh_running.is_ok(), "A refresh should reach the runner");

    let resolved_b = resolver
        .resolve(
            &item("compatible-refresh-b"),
            None,
            CancellationToken::new(),
        )
        .await;
    assert!(resolved_b.is_ok(), "B should evict A's old cache entry");

    let reads_before_missing = clock.read_count();
    let missing_resolver = resolver.clone();
    let missing_media = media_a.clone();
    let missing = tokio::spawn(async move {
        missing_resolver
            .resolve(&missing_media, None, CancellationToken::new())
            .await
    });
    let missing_observed = tokio::time::timeout(
        Duration::from_secs(1),
        clock.wait_for_reads(reads_before_missing + 1),
    )
    .await;
    assert!(
        missing_observed.is_ok(),
        "UseCache(A) should observe the eviction"
    );
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    release_refresh.notify_one();

    let refresh = match refresh.await {
        Ok(result) => result,
        Err(error) => panic!("refresh task should not panic: {error}"),
    };
    let missing = match missing.await {
        Ok(result) => result,
        Err(error) => panic!("missing-cache task should not panic: {error}"),
    };
    assert_eq!(missing, refresh);
    assert_eq!(
        refresh.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/a-refreshed"))
    );

    let calls = runner.calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls
            .iter()
            .filter(|call| called_video_id(call).as_deref() == Some("compatible-refresh-a"))
            .count(),
        2,
        "A should have one prime and one refresh process"
    );
}

#[tokio::test]
async fn zero_ttl_always_misses_the_cache() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/first")),
        FakeResponse::Output(stream_output("http://127.0.0.1/second")),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );
    let media = item("zero-ttl");

    let first = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    let second = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;

    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn clock_reversal_conservatively_expires_the_cached_entry() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/first")),
        FakeResponse::Output(stream_output("http://127.0.0.1/after-reversal")),
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_mins(1),
    );
    let media = item("clock-reversal");

    let first = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    assert!(first.is_ok());
    clock.set(fixed_time() - time::Duration::days(1));
    let reversed = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;

    assert_eq!(
        reversed.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/after-reversal"))
    );
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn unrepresentable_ttl_conservatively_misses_the_cache() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/first")),
        FakeResponse::Output(stream_output("http://127.0.0.1/second")),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::MAX,
    );
    let media = item("huge-ttl");

    let first = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    let second = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;

    assert!(first.is_ok());
    assert_eq!(
        second.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/second"))
    );
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn anonymous_and_each_auth_identity_have_isolated_cache_entries() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/anonymous")),
        FakeResponse::Output(stream_output("http://127.0.0.1/account-a")),
        FakeResponse::Output(stream_output("http://127.0.0.1/account-b")),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    );
    let media = item("auth-cache");
    let account_a = CookieFile::new("/tmp/account-a.cookies", identity("account-a"));
    let same_account_different_path =
        CookieFile::new("/tmp/account-a-new.cookies", identity("account-a"));
    let account_b = CookieFile::new("/tmp/account-b.cookies", identity("account-b"));

    let anonymous = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    let first_a = resolver
        .resolve(&media, Some(&account_a), CancellationToken::new())
        .await;
    let cached_a = resolver
        .resolve(
            &media,
            Some(&same_account_different_path),
            CancellationToken::new(),
        )
        .await;
    let first_b = resolver
        .resolve(&media, Some(&account_b), CancellationToken::new())
        .await;

    assert!(anonymous.is_ok());
    assert!(first_a.is_ok());
    assert_eq!(cached_a, first_a);
    assert!(first_b.is_ok());
    assert_eq!(runner.call_count(), 3);
}

#[tokio::test]
async fn small_cache_capacity_evicts_the_oldest_generation() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/a-first")),
        FakeResponse::Output(stream_output("http://127.0.0.1/b")),
        FakeResponse::Output(stream_output("http://127.0.0.1/c")),
        FakeResponse::Output(stream_output("http://127.0.0.1/a-rerun")),
    ]));
    let resolver = YtDlpResolver::with_cache_capacity(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
        2,
    );
    let first_a = resolver
        .resolve(&item("capacity-a"), None, CancellationToken::new())
        .await;
    let first_b = resolver
        .resolve(&item("capacity-b"), None, CancellationToken::new())
        .await;
    let first_c = resolver
        .resolve(&item("capacity-c"), None, CancellationToken::new())
        .await;

    assert!(first_a.is_ok());
    assert!(first_b.is_ok());
    assert!(first_c.is_ok());
    assert!(
        resolver
            .resolve(&item("capacity-b"), None, CancellationToken::new())
            .await
            .is_ok()
    );
    assert!(
        resolver
            .resolve(&item("capacity-c"), None, CancellationToken::new())
            .await
            .is_ok()
    );
    let rerun_a = resolver
        .resolve(&item("capacity-a"), None, CancellationToken::new())
        .await;
    assert_eq!(
        rerun_a.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/a-rerun"))
    );
    assert_eq!(runner.call_count(), 4);
}

#[tokio::test]
async fn insertion_prunes_all_reversed_entries_before_evicting_a_live_oldest_entry() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(stream_output("http://127.0.0.1/a-live")),
        FakeResponse::Output(stream_output("http://127.0.0.1/b-future")),
        FakeResponse::Output(stream_output("http://127.0.0.1/c-future")),
        FakeResponse::Output(stream_output("http://127.0.0.1/d-future")),
        FakeResponse::Output(stream_output("http://127.0.0.1/e")),
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = YtDlpResolver::with_cache_capacity(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_secs(200),
        4,
    );
    let live_a = resolver
        .resolve(&item("prune-a"), None, CancellationToken::new())
        .await;
    assert!(live_a.is_ok());

    clock.set(fixed_time() + time::Duration::seconds(150));
    for video_id in ["prune-b", "prune-c", "prune-d"] {
        let result = resolver
            .resolve(&item(video_id), None, CancellationToken::new())
            .await;
        assert!(result.is_ok());
    }

    clock.set(fixed_time() + time::Duration::seconds(100));
    let inserted = resolver
        .resolve(&item("prune-e"), None, CancellationToken::new())
        .await;
    let still_cached_a = resolver
        .resolve(&item("prune-a"), None, CancellationToken::new())
        .await;

    assert!(inserted.is_ok());
    assert_eq!(still_cached_a, live_a);
    assert_eq!(runner.call_count(), 5);
}

#[tokio::test]
async fn failures_are_not_cached() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(output(0, b"{bad-json".to_vec(), Vec::new())),
        FakeResponse::Output(stream_output("http://127.0.0.1/recovered")),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    );
    let media = item("retry-after-failure");

    let first = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    let second = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;

    assert!(first.is_err());
    assert!(second.is_ok());
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn cancellation_before_lookup_wins_even_when_cache_is_live() {
    let runner = Arc::new(FakeRunner::new([FakeResponse::Output(stream_output(
        "http://127.0.0.1/cached",
    ))]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    );
    let media = item("cancel-before-cache");
    let primed = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    assert!(primed.is_ok());
    let cancelled = CancellationToken::new();
    cancelled.cancel();

    let Err(error) = resolver.resolve(&media, None, cancelled).await else {
        panic!("pre-cancelled request must not use cache");
    };

    assert_eq!(error.category(), ResolveErrorCategory::Cancellation);
    assert_eq!(runner.call_count(), 1);
}

#[tokio::test]
async fn concurrent_same_key_cache_misses_share_one_process_result() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([FakeResponse::Blocked {
        started: started.clone(),
        release: release.clone(),
        output: stream_output("http://127.0.0.1/shared"),
    }]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = Arc::new(YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_mins(1),
    ));
    let media = item("single-flight");

    let first_resolver = resolver.clone();
    let first_media = media.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve(&first_media, None, CancellationToken::new())
            .await
    });
    let runner_started = tokio::time::timeout(Duration::from_secs(1), started.notified()).await;
    assert!(runner_started.is_ok(), "first miss should start runner");

    let second_resolver = resolver.clone();
    let second_media = media.clone();
    let second = tokio::spawn(async move {
        second_resolver
            .resolve(&second_media, None, CancellationToken::new())
            .await
    });
    let both_looked_up =
        tokio::time::timeout(Duration::from_secs(1), clock.wait_for_reads(3)).await;
    assert!(
        both_looked_up.is_ok(),
        "second miss should reach the concurrency gate"
    );
    release.notify_one();

    let first = match first.await {
        Ok(result) => result,
        Err(error) => panic!("first miss task should not panic: {error}"),
    };
    let second = match second.await {
        Ok(result) => result,
        Err(error) => panic!("second miss task should not panic: {error}"),
    };
    assert_eq!(first, second);
    assert_eq!(runner.call_count(), 1);
}

#[tokio::test]
async fn concurrent_same_key_zero_ttl_callers_share_one_success() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([FakeResponse::Blocked {
        started: started.clone(),
        release: release.clone(),
        output: stream_output("http://127.0.0.1/zero-ttl-shared"),
    }]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = Arc::new(YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::ZERO,
    ));
    let media = item("zero-ttl-flight");

    let first_resolver = resolver.clone();
    let first_media = media.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve(&first_media, None, CancellationToken::new())
            .await
    });
    let runner_started = tokio::time::timeout(Duration::from_secs(1), started.notified()).await;
    assert!(runner_started.is_ok(), "first caller should start runner");

    let second_resolver = resolver.clone();
    let second_media = media.clone();
    let second = tokio::spawn(async move {
        second_resolver
            .resolve(&second_media, None, CancellationToken::new())
            .await
    });
    let both_joined = tokio::time::timeout(Duration::from_secs(1), clock.wait_for_reads(3)).await;
    assert!(both_joined.is_ok(), "second caller should join the flight");
    release.notify_one();

    let first = match first.await {
        Ok(result) => result,
        Err(error) => panic!("first caller should not panic: {error}"),
    };
    let second = match second.await {
        Ok(result) => result,
        Err(error) => panic!("second caller should not panic: {error}"),
    };
    assert_eq!(first, second);
    assert_eq!(
        first.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/zero-ttl-shared"))
    );
    assert_eq!(runner.call_count(), 1);
}

#[tokio::test]
async fn concurrent_same_key_callers_share_failure_then_later_retry_runs_again() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Blocked {
            started: started.clone(),
            release: release.clone(),
            output: output(7, Vec::new(), b"fixture extractor failure".to_vec()),
        },
        FakeResponse::Output(stream_output("http://127.0.0.1/retry")),
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = Arc::new(YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_mins(1),
    ));
    let media = item("shared-failure");

    let first_resolver = resolver.clone();
    let first_media = media.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve(&first_media, None, CancellationToken::new())
            .await
    });
    let runner_started = tokio::time::timeout(Duration::from_secs(1), started.notified()).await;
    assert!(runner_started.is_ok(), "first caller should start runner");

    let second_resolver = resolver.clone();
    let second_media = media.clone();
    let second = tokio::spawn(async move {
        second_resolver
            .resolve(&second_media, None, CancellationToken::new())
            .await
    });
    let both_joined = tokio::time::timeout(Duration::from_secs(1), clock.wait_for_reads(3)).await;
    assert!(both_joined.is_ok(), "second caller should join the flight");
    release.notify_one();

    let first = match first.await {
        Ok(result) => result,
        Err(error) => panic!("first caller should not panic: {error}"),
    };
    let second = match second.await {
        Ok(result) => result,
        Err(error) => panic!("second caller should not panic: {error}"),
    };
    let Err(first_error) = first else {
        panic!("shared extractor failure must fail");
    };
    let Err(second_error) = second else {
        panic!("shared extractor failure must fail");
    };
    assert_eq!(first_error, second_error);
    assert_eq!(first_error.category(), ResolveErrorCategory::Extractor);
    assert_eq!(runner.call_count(), 1);

    let retry = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    assert!(retry.is_ok(), "later caller should create a new flight");
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn joined_waiter_keeps_shared_result_after_persistent_cache_eviction() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Blocked {
            started: started.clone(),
            release: release.clone(),
            output: stream_output("http://127.0.0.1/shared-before-eviction"),
        },
        FakeResponse::Output(stream_output("http://127.0.0.1/evicting-key")),
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = Arc::new(YtDlpResolver::with_cache_capacity(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_mins(1),
        1,
    ));
    let media = item("evicted-flight-result");

    let first_resolver = resolver.clone();
    let first_media = media.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve(&first_media, None, CancellationToken::new())
            .await
    });
    let runner_started = tokio::time::timeout(Duration::from_secs(1), started.notified()).await;
    assert!(runner_started.is_ok(), "first caller should start runner");

    let delayed_resolver = resolver.clone();
    let delayed_media = media.clone();
    let delayed = tokio::spawn(async move {
        delayed_resolver
            .resolve(&delayed_media, None, CancellationToken::new())
            .await
    });
    let both_joined = tokio::time::timeout(Duration::from_secs(1), clock.wait_for_reads(3)).await;
    assert!(both_joined.is_ok(), "delayed caller should join the flight");
    release.notify_one();

    let first = match first.await {
        Ok(result) => result,
        Err(error) => panic!("first caller should not panic: {error}"),
    };
    assert!(first.is_ok());
    let evicting = resolver
        .resolve(&item("evicting-key"), None, CancellationToken::new())
        .await;
    assert!(evicting.is_ok());

    let delayed = match delayed.await {
        Ok(result) => result,
        Err(error) => panic!("delayed caller should not panic: {error}"),
    };
    assert_eq!(delayed, first);
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn cancelling_one_waiter_keeps_the_shared_flight_for_another() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Blocked {
            started: started.clone(),
            release: release.clone(),
            output: stream_output("http://127.0.0.1/shared-after-cancel"),
        },
        FakeResponse::Output(stream_output("http://127.0.0.1/duplicate")),
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = Arc::new(YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_mins(1),
    ));
    let media = item("waiter-cancellation");
    let first_cancel = CancellationToken::new();

    let first_resolver = resolver.clone();
    let first_media = media.clone();
    let first_token = first_cancel.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve(&first_media, None, first_token)
            .await
    });
    let runner_started = tokio::time::timeout(Duration::from_secs(1), started.notified()).await;
    assert!(runner_started.is_ok(), "first waiter should start runner");

    let second_resolver = resolver.clone();
    let second_media = media.clone();
    let second = tokio::spawn(async move {
        second_resolver
            .resolve(&second_media, None, CancellationToken::new())
            .await
    });
    let both_joined = tokio::time::timeout(Duration::from_secs(1), clock.wait_for_reads(3)).await;
    assert!(both_joined.is_ok(), "second waiter should join the flight");
    first_cancel.cancel();
    tokio::task::yield_now().await;
    release.notify_one();

    let first = match first.await {
        Ok(result) => result,
        Err(error) => panic!("cancelled waiter should not panic: {error}"),
    };
    let second = match second.await {
        Ok(result) => result,
        Err(error) => panic!("remaining waiter should not panic: {error}"),
    };
    let Err(error) = first else {
        panic!("cancelled waiter must receive cancellation");
    };
    assert_eq!(error.category(), ResolveErrorCategory::Cancellation);
    assert_eq!(
        second.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/shared-after-cancel"))
    );
    assert_eq!(runner.call_count(), 1);
}

#[tokio::test]
async fn cancellation_while_waiting_for_the_process_gate_never_runs_or_caches() {
    let first_started = Arc::new(Notify::new());
    let second_started = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Pending(first_started.clone()),
        FakeResponse::Pending(second_started.clone()),
    ]));
    let clock = Arc::new(FakeClock::new(fixed_time()));
    let resolver = Arc::new(YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        clock.clone(),
        Duration::from_mins(1),
    ));

    let first_blocker_resolver = resolver.clone();
    let first_blocker = tokio::spawn(async move {
        first_blocker_resolver
            .resolve(&item("gate-blocker-a"), None, CancellationToken::new())
            .await
    });
    let first_runner_started =
        tokio::time::timeout(Duration::from_secs(1), first_started.notified()).await;
    assert!(
        first_runner_started.is_ok(),
        "first blocker should hold a process permit"
    );

    let second_blocker_resolver = resolver.clone();
    let second_blocker = tokio::spawn(async move {
        second_blocker_resolver
            .resolve(&item("gate-blocker-b"), None, CancellationToken::new())
            .await
    });
    let second_runner_started =
        tokio::time::timeout(Duration::from_secs(1), second_started.notified()).await;
    assert!(
        second_runner_started.is_ok(),
        "second blocker should hold the other process permit"
    );

    let waiting_cancel = CancellationToken::new();
    let waiting_resolver = resolver.clone();
    let waiting_token = waiting_cancel.clone();
    let waiting = tokio::spawn(async move {
        waiting_resolver
            .resolve(&item("gate-waiter"), None, waiting_token)
            .await
    });
    let waiter_looked_up =
        tokio::time::timeout(Duration::from_secs(1), clock.wait_for_reads(5)).await;
    assert!(
        waiter_looked_up.is_ok(),
        "waiter should reach the process gate"
    );
    tokio::task::yield_now().await;
    waiting_cancel.cancel();

    let waiting_result = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap_or_else(|error| panic!("gate cancellation should be prompt: {error}"));
    let waiting_result = match waiting_result {
        Ok(result) => result,
        Err(error) => panic!("waiting task should not panic: {error}"),
    };
    let Err(error) = waiting_result else {
        panic!("cancelled gate waiter must fail");
    };
    assert_eq!(error.category(), ResolveErrorCategory::Cancellation);
    assert_eq!(runner.call_count(), 2);

    first_blocker.abort();
    second_blocker.abort();
    let first_blocker_join = first_blocker.await;
    let second_blocker_join = second_blocker.await;
    assert!(
        first_blocker_join.is_err() && second_blocker_join.is_err(),
        "aborted blocking tasks should not finish normally"
    );
}

#[tokio::test]
async fn different_keys_can_run_concurrently_within_the_global_bound() {
    let first_started = Arc::new(Notify::new());
    let second_started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Blocked {
            started: first_started.clone(),
            release: release.clone(),
            output: stream_output("http://127.0.0.1/parallel-a"),
        },
        FakeResponse::Blocked {
            started: second_started.clone(),
            release: release.clone(),
            output: stream_output("http://127.0.0.1/parallel-b"),
        },
    ]));
    let resolver = Arc::new(YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    ));

    let first_resolver = resolver.clone();
    let first = tokio::spawn(async move {
        first_resolver
            .resolve(&item("parallel-a"), None, CancellationToken::new())
            .await
    });
    let first_ready = tokio::time::timeout(Duration::from_secs(1), first_started.notified()).await;
    assert!(first_ready.is_ok(), "first key should start");

    let second_resolver = resolver.clone();
    let second = tokio::spawn(async move {
        second_resolver
            .resolve(&item("parallel-b"), None, CancellationToken::new())
            .await
    });
    let second_ready =
        tokio::time::timeout(Duration::from_secs(1), second_started.notified()).await;
    assert!(
        second_ready.is_ok(),
        "an unrelated key must not wait behind the first process"
    );
    assert_eq!(runner.call_count(), 2);
    release.notify_waiters();

    let first = match first.await {
        Ok(result) => result,
        Err(error) => panic!("first key should not panic: {error}"),
    };
    let second = match second.await {
        Ok(result) => result,
        Err(error) => panic!("second key should not panic: {error}"),
    };
    assert!(first.is_ok());
    assert!(second.is_ok());
}

#[tokio::test]
async fn in_flight_cancellation_returns_promptly_and_does_not_cache() {
    let started = Arc::new(Notify::new());
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Pending(started.clone()),
        FakeResponse::Output(stream_output("http://127.0.0.1/after-cancel")),
    ]));
    let resolver = Arc::new(YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    ));
    let media = item("cancel-in-flight");
    let cancel = CancellationToken::new();
    let wait_for_start = started.notified();
    let task_resolver = resolver.clone();
    let task_media = media.clone();
    let task_cancel = cancel.clone();
    let task =
        tokio::spawn(async move { task_resolver.resolve(&task_media, None, task_cancel).await });

    let started_result = tokio::time::timeout(Duration::from_secs(1), wait_for_start).await;
    assert!(started_result.is_ok(), "fake runner should start");
    cancel.cancel();
    let joined = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap_or_else(|error| panic!("cancellation should stop promptly: {error}"));
    let result = match joined {
        Ok(result) => result,
        Err(error) => panic!("resolution task should not panic: {error}"),
    };
    let Err(error) = result else {
        panic!("cancelled resolution must fail");
    };
    assert_eq!(error.category(), ResolveErrorCategory::Cancellation);

    let retry = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    assert!(retry.is_ok(), "cancelled result must not populate cache");
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn cancellation_just_before_runner_success_wins_and_never_populates_cache() {
    let cancel = CancellationToken::new();
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::CancelThenOutput {
            cancel: cancel.clone(),
            output: stream_output("http://127.0.0.1/must-not-cache"),
        },
        FakeResponse::Output(stream_output("http://127.0.0.1/retry")),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner.clone(),
        Arc::new(FakeClock::new(fixed_time())),
        Duration::from_mins(1),
    );
    let media = item("cancel-at-runner-completion");

    let Err(error) = resolver.resolve(&media, None, cancel).await else {
        panic!("runner-side cancellation must win over successful output");
    };
    assert_eq!(error.category(), ResolveErrorCategory::Cancellation);

    let retry = resolver
        .resolve(&media, None, CancellationToken::new())
        .await;
    assert_eq!(
        retry.as_ref().map(|stream| &stream.url),
        Ok(&parsed_url("http://127.0.0.1/retry"))
    );
    assert_eq!(runner.call_count(), 2);
}

#[tokio::test]
async fn process_failures_map_to_a_stable_category_without_source_leaks() {
    let secret_program = PathBuf::from("/private/cookie/account/yt-dlp");
    let source = io::Error::other("spawn failed near https://user:pass@127.0.0.1/?token=secret");
    let runner = Arc::new(FakeRunner::new([FakeResponse::Error(
        ProcessError::Spawn {
            program: secret_program.clone(),
            source,
        },
    )]));
    let resolver = YtDlpResolver::new(
        secret_program.clone(),
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );

    let Err(error) = resolver
        .resolve(&item("process-error"), None, CancellationToken::new())
        .await
    else {
        panic!("runner failure must fail resolution");
    };

    assert_eq!(error.category(), ResolveErrorCategory::Process);
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(!rendered.contains("private/cookie"));
        assert!(!rendered.contains("user"));
        assert!(!rendered.contains("pass"));
        assert!(!rendered.contains("token=secret"));
    }
}

#[tokio::test]
async fn resolver_trait_is_object_safe() {
    let runner = Arc::new(FakeRunner::new([
        FakeResponse::Output(fixture_output()),
        FakeResponse::Output(fixture_output()),
    ]));
    let resolver = YtDlpResolver::new(
        "yt-dlp",
        runner,
        Arc::new(FakeClock::new(fixed_time())),
        Duration::ZERO,
    );
    let service: &dyn Resolver = &resolver;

    let result = service
        .resolve(&item("object-safe"), None, CancellationToken::new())
        .await;
    let policy_result = service
        .resolve_with_policy(
            &item("object-safe"),
            None,
            ResolvePolicy::ForceRefresh,
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_ok());
    assert!(policy_result.is_ok());
}
