use std::{
    collections::{HashMap, VecDeque},
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Mutex,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ytermusic::{
    cli::execute_doctor,
    diagnostics::{DependencyChecker, DiagnosticStatus, Platform, sanitize},
    process::{
        CommandSpec, ExecutableLocator, LocatorError, OutputStream, ProcessError, ProcessLimits,
        ProcessOutput, ProcessRunner, TokioProcessRunner,
    },
};

#[cfg(unix)]
fn exit_status(code: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    ExitStatus::from_raw(code << 8)
}

#[cfg(windows)]
fn exit_status(code: i32) -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;

    #[expect(clippy::cast_sign_loss, reason = "test exit codes are non-negative")]
    ExitStatus::from_raw(code as u32)
}

fn output(code: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> ProcessOutput {
    ProcessOutput {
        status: exit_status(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

#[test]
#[ignore = "child process fixture"]
fn tokio_runner_child_output_fixture() {
    println!("ytermusic-child-stdout-marker");
    eprintln!("ytermusic-child-stderr-marker");
}

#[test]
#[ignore = "child process fixture"]
fn tokio_runner_child_hang_fixture() {
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
#[ignore = "child process fixture"]
fn tokio_runner_child_large_stdout_fixture() {
    println!("{}", "stdout-limit-marker".repeat(4_096));
}

#[test]
#[ignore = "child process fixture"]
fn tokio_runner_child_large_stderr_fixture() {
    eprintln!("{}", "stderr-limit-marker".repeat(4_096));
}

fn child_fixture_spec(name: &str, limits: ProcessLimits) -> CommandSpec {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => panic!("current test executable should be available: {error}"),
    };
    CommandSpec::new(
        executable,
        ["--ignored", "--exact", name, "--nocapture"]
            .into_iter()
            .map(OsString::from),
    )
    .with_limits(limits)
}

#[tokio::test]
async fn tokio_runner_captures_child_stdout_and_stderr() {
    let defaults = ProcessLimits::default();
    assert_eq!(defaults.timeout, Duration::from_secs(10));
    assert_eq!(defaults.max_stdout_bytes, 1_048_576);
    assert_eq!(defaults.max_stderr_bytes, 1_048_576);
    let spec = child_fixture_spec("tokio_runner_child_output_fixture", defaults);

    let captured = match TokioProcessRunner.output(spec).await {
        Ok(captured) => captured,
        Err(error) => panic!("fixture process should execute: {error}"),
    };

    assert!(captured.status.success());
    assert!(
        String::from_utf8_lossy(&captured.stdout).contains("ytermusic-child-stdout-marker"),
        "child stdout must be returned instead of inherited"
    );
    assert!(
        String::from_utf8_lossy(&captured.stderr).contains("ytermusic-child-stderr-marker"),
        "child stderr must be returned instead of inherited"
    );
}

#[tokio::test]
async fn tokio_runner_times_out_and_reaps_a_hung_child_promptly() {
    let limits = ProcessLimits {
        timeout: Duration::from_millis(150),
        max_stdout_bytes: 1_024,
        max_stderr_bytes: 1_024,
    };
    let spec = child_fixture_spec("tokio_runner_child_hang_fixture", limits);
    let started = Instant::now();

    let Err(error) = TokioProcessRunner.output(spec).await else {
        panic!("hung child must time out");
    };

    assert!(matches!(
        error,
        ProcessError::Timeout {
            timeout,
            ..
        } if timeout == Duration::from_millis(150)
    ));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "timeout must terminate and reap promptly"
    );
}

#[tokio::test]
async fn tokio_runner_bounds_stdout_without_allocating_the_full_stream() {
    let limits = ProcessLimits {
        timeout: Duration::from_secs(5),
        max_stdout_bytes: 1_024,
        max_stderr_bytes: 1_048_576,
    };
    let spec = child_fixture_spec("tokio_runner_child_large_stdout_fixture", limits);

    let Err(error) = TokioProcessRunner.output(spec).await else {
        panic!("oversized stdout must fail");
    };

    assert!(matches!(
        error,
        ProcessError::OutputLimitExceeded {
            stream: OutputStream::Stdout,
            limit: 1_024,
            ..
        }
    ));
}

#[tokio::test]
async fn tokio_runner_bounds_stderr_without_allocating_the_full_stream() {
    let limits = ProcessLimits {
        timeout: Duration::from_secs(5),
        max_stdout_bytes: 1_048_576,
        max_stderr_bytes: 1_024,
    };
    let spec = child_fixture_spec("tokio_runner_child_large_stderr_fixture", limits);

    let Err(error) = TokioProcessRunner.output(spec).await else {
        panic!("oversized stderr must fail");
    };

    assert!(matches!(
        error,
        ProcessError::OutputLimitExceeded {
            stream: OutputStream::Stderr,
            limit: 1_024,
            ..
        }
    ));
}

#[derive(Default)]
struct FakeLocator {
    paths: HashMap<String, PathBuf>,
    errors: HashMap<String, String>,
}

impl FakeLocator {
    fn with_path(mut self, executable: &str, path: impl Into<PathBuf>) -> Self {
        self.paths.insert(executable.to_owned(), path.into());
        self
    }

    fn with_error(mut self, executable: &str, message: &str) -> Self {
        self.errors
            .insert(executable.to_owned(), message.to_owned());
        self
    }
}

impl ExecutableLocator for FakeLocator {
    fn find(&self, executable: &str) -> Result<Option<PathBuf>, LocatorError> {
        if let Some(message) = self.errors.get(executable) {
            return Err(LocatorError::Lookup {
                executable: executable.to_owned(),
                message: message.clone(),
            });
        }

        Ok(self.paths.get(executable).cloned())
    }
}

enum FakeResponse {
    Output(ProcessOutput),
    Error(String),
}

struct FakeRunner {
    responses: Mutex<VecDeque<FakeResponse>>,
    calls: Mutex<Vec<CommandSpec>>,
}

impl FakeRunner {
    fn new(responses: impl IntoIterator<Item = ProcessOutput>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().map(FakeResponse::Output).collect()),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_responses(responses: impl IntoIterator<Item = FakeResponse>) -> Self {
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
}

#[async_trait]
impl ProcessRunner for FakeRunner {
    async fn output(&self, spec: CommandSpec) -> Result<ProcessOutput, ProcessError> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(spec.clone());

        match self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected process call: {spec:?}"))
        {
            FakeResponse::Output(output) => Ok(output),
            FakeResponse::Error(message) => Err(ProcessError::Spawn {
                program: spec.program,
                source: io::Error::other(message),
            }),
        }
    }
}

fn all_dependencies() -> FakeLocator {
    FakeLocator::default()
        .with_path("mpv", "/opt/Media Tools/mpv;still-one-program")
        .with_path("yt-dlp", "/opt/Media Tools/yt-dlp & helper")
        .with_path("ffmpeg", "/opt/Media Tools/ffmpeg $(safe)")
}

fn healthy_outputs() -> Vec<ProcessOutput> {
    vec![
        output(0, b"mpv 0.39.0 Copyright".to_vec(), Vec::new()),
        output(0, b"--input-ipc-server=File".to_vec(), Vec::new()),
        output(0, b"2026.07.04\n".to_vec(), Vec::new()),
        output(0, b"-J, --dump-single-json".to_vec(), Vec::new()),
        output(0, b"ffmpeg version 7.1.1 Copyright".to_vec(), Vec::new()),
    ]
}

#[tokio::test]
async fn healthy_report_is_deterministic_and_keeps_os_arguments_separate() {
    let locator = all_dependencies();
    let runner = FakeRunner::new(healthy_outputs());
    let report = DependencyChecker::new(&locator, &runner, Platform::MacOs)
        .check()
        .await;

    assert!(report.playback_available());
    assert!(report.browsing_available());
    assert_eq!(report.exit_code(), 0);
    assert_eq!(
        report
            .rows()
            .iter()
            .map(ytermusic::diagnostics::DiagnosticRow::component)
            .collect::<Vec<_>>(),
        ["browsing", "mpv", "yt-dlp", "ffmpeg", "playback"]
    );
    assert!(
        report
            .rows()
            .iter()
            .all(|row| row.status() == DiagnosticStatus::Healthy)
    );

    assert_eq!(
        report.render(),
        concat!(
            "COMPONENT  STATUS     DETAILS\n",
            "browsing   healthy    metadata browsing available\n",
            "mpv        healthy    0.39.0 | /opt/Media Tools/mpv;still-one-program\n",
            "yt-dlp     healthy    2026.07.04 | /opt/Media Tools/yt-dlp & helper\n",
            "ffmpeg     healthy    7.1.1 | /opt/Media Tools/ffmpeg $(safe)\n",
            "playback   healthy    ready\n",
        )
    );

    assert_eq!(
        runner.calls(),
        vec![
            CommandSpec::new(
                "/opt/Media Tools/mpv;still-one-program",
                ["--no-config", "--version"].into_iter().map(OsString::from),
            ),
            CommandSpec::new(
                "/opt/Media Tools/mpv;still-one-program",
                ["--no-config", "--list-options"]
                    .into_iter()
                    .map(OsString::from),
            ),
            CommandSpec::new(
                "/opt/Media Tools/yt-dlp & helper",
                ["--ignore-config", "--version"]
                    .into_iter()
                    .map(OsString::from),
            ),
            CommandSpec::new(
                "/opt/Media Tools/yt-dlp & helper",
                ["--ignore-config", "--help"]
                    .into_iter()
                    .map(OsString::from),
            ),
            CommandSpec::new(
                "/opt/Media Tools/ffmpeg $(safe)",
                [OsString::from("-version")],
            ),
        ]
    );
    assert!(runner.calls().iter().all(|call| {
        !matches!(
            call.program.file_name().and_then(OsStr::to_str),
            Some("sh" | "bash" | "zsh" | "cmd" | "cmd.exe" | "powershell")
        )
    }));
}

#[tokio::test]
async fn missing_dependency_uses_the_injected_platform_hint() {
    let cases = [
        (Platform::MacOs, "brew install mpv"),
        (
            Platform::Linux,
            "install mpv with your distribution's package manager",
        ),
        (Platform::Windows, "winget install --id=shinchiro.mpv"),
        (Platform::Other, "install mpv and add it to PATH"),
    ];

    for (platform, expected_hint) in cases {
        let locator = FakeLocator::default()
            .with_path("yt-dlp", "/tools/yt-dlp")
            .with_path("ffmpeg", "/tools/ffmpeg");
        let runner = FakeRunner::new([
            output(0, b"2026.07.04".to_vec(), Vec::new()),
            output(0, b"--dump-single-json".to_vec(), Vec::new()),
            output(0, b"ffmpeg version 7.1.1".to_vec(), Vec::new()),
        ]);

        let report = DependencyChecker::new(&locator, &runner, platform)
            .check()
            .await;
        let mpv = report
            .row("mpv")
            .unwrap_or_else(|| panic!("mpv row must be present for {platform:?}"));

        assert_eq!(mpv.status(), DiagnosticStatus::Unhealthy);
        assert!(
            mpv.detail()
                .to_ascii_lowercase()
                .contains(&expected_hint.to_ascii_lowercase()),
            "unexpected hint for {platform:?}: {}",
            mpv.detail()
        );
        assert!(!report.playback_available());
        assert!(report.browsing_available());
        assert_eq!(report.exit_code(), 1);
    }
}

#[tokio::test]
async fn failed_or_missing_capabilities_make_playback_unavailable() {
    let runner = FakeRunner::new([
        output(0, b"mpv 0.39.0".to_vec(), Vec::new()),
        output(0, b"options without ipc".to_vec(), Vec::new()),
        output(0, b"2026.07.04".to_vec(), Vec::new()),
        output(3, Vec::new(), b"help failed".to_vec()),
        output(2, Vec::new(), b"version failed".to_vec()),
    ]);

    let report = DependencyChecker::new(&all_dependencies(), &runner, Platform::Linux)
        .check()
        .await;

    for dependency in ["mpv", "yt-dlp", "ffmpeg"] {
        assert_eq!(
            report
                .row(dependency)
                .map(ytermusic::diagnostics::DiagnosticRow::status),
            Some(DiagnosticStatus::Unhealthy)
        );
    }
    assert!(!report.playback_available());
    assert_eq!(report.exit_code(), 1);
    assert!(!report.render().contains("help failed"));
    assert!(!report.render().contains("version failed"));
}

#[tokio::test]
async fn yt_dlp_requires_an_actual_json_command_line_flag() {
    let runner = FakeRunner::new([
        output(0, b"mpv 0.39.0".to_vec(), Vec::new()),
        output(0, b"input-ipc-server".to_vec(), Vec::new()),
        output(0, b"2026.07.04".to_vec(), Vec::new()),
        output(0, b"JSON is represented by J".to_vec(), Vec::new()),
        output(0, b"ffmpeg version 7.1.1".to_vec(), Vec::new()),
    ]);

    let report = DependencyChecker::new(&all_dependencies(), &runner, Platform::Linux)
        .check()
        .await;

    assert_eq!(
        report
            .row("yt-dlp")
            .map(ytermusic::diagnostics::DiagnosticRow::status),
        Some(DiagnosticStatus::Unhealthy)
    );
    assert!(!report.playback_available());
}

#[tokio::test]
async fn capable_tools_with_unknown_versions_are_degraded_but_usable() {
    let runner = FakeRunner::new([
        output(0, b"mpv rolling release".to_vec(), Vec::new()),
        output(0, b"input-ipc-server".to_vec(), Vec::new()),
        output(0, b"nightly build".to_vec(), Vec::new()),
        output(0, b"-J".to_vec(), Vec::new()),
        output(0, b"ffmpeg from tomorrow".to_vec(), Vec::new()),
    ]);

    let report = DependencyChecker::new(&all_dependencies(), &runner, Platform::Other)
        .check()
        .await;

    for dependency in ["mpv", "yt-dlp", "ffmpeg"] {
        assert_eq!(
            report
                .row(dependency)
                .map(ytermusic::diagnostics::DiagnosticRow::status),
            Some(DiagnosticStatus::Degraded)
        );
    }
    assert!(report.playback_available());
    assert_eq!(report.exit_code(), 0);
}

#[tokio::test]
async fn ffmpeg_accepts_two_part_versions_and_rejects_one_or_four_part_runs() {
    let cases = [
        ("ffmpeg version 8.0", DiagnosticStatus::Healthy),
        ("ffmpeg version 8", DiagnosticStatus::Degraded),
        ("ffmpeg version 8.0.1.2", DiagnosticStatus::Degraded),
    ];

    for (ffmpeg_output, expected_status) in cases {
        let runner = FakeRunner::new([
            output(0, b"mpv 0.39.0".to_vec(), Vec::new()),
            output(0, b"input-ipc-server".to_vec(), Vec::new()),
            output(0, b"2026.07.04".to_vec(), Vec::new()),
            output(0, b"--dump-single-json".to_vec(), Vec::new()),
            output(0, ffmpeg_output.as_bytes().to_vec(), Vec::new()),
        ]);
        let report = DependencyChecker::new(&all_dependencies(), &runner, Platform::Linux)
            .check()
            .await;
        let ffmpeg = report
            .row("ffmpeg")
            .unwrap_or_else(|| panic!("ffmpeg row must be present for {ffmpeg_output}"));

        assert_eq!(ffmpeg.status(), expected_status, "{ffmpeg_output}");
        if expected_status == DiagnosticStatus::Healthy {
            assert!(ffmpeg.detail().starts_with("8.0 |"));
        }
    }
}

#[tokio::test]
async fn process_and_locator_errors_are_compact_and_redacted() {
    let locator = FakeLocator::default()
        .with_error(
            "mpv",
            "Authorization: Bearer locate-secret https://example.test/a?sig=locate-secret",
        )
        .with_path("yt-dlp", "/tools/yt-dlp")
        .with_path("ffmpeg", "/tools/ffmpeg");
    let runner = FakeRunner::with_responses([
        FakeResponse::Error(
            "COOKIE=session-secret https://media.test/audio?token=stream-secret".to_owned(),
        ),
        FakeResponse::Output(output(0, b"--dump-single-json".to_vec(), Vec::new())),
        FakeResponse::Output(output(
            0,
            [b"ffmpeg version 7.".as_slice(), &[0xff, 0xfe], b".1"].concat(),
            Vec::new(),
        )),
    ]);

    let report = DependencyChecker::new(&locator, &runner, Platform::MacOs)
        .check()
        .await;
    let structured_debug = format!("{report:?}");
    let rendered = report.render();

    assert!(rendered.contains("[REDACTED]"));
    for secret in ["locate-secret", "session-secret", "stream-secret"] {
        assert!(
            report
                .rows()
                .iter()
                .all(|row| !row.detail().contains(secret)),
            "structured row leaked {secret}"
        );
        assert!(!structured_debug.contains(secret));
        assert!(!rendered.contains(secret));
    }
    assert!(
        report
            .rows()
            .iter()
            .any(|row| row.detail().contains("[REDACTED]"))
    );
    assert!(!structured_debug.contains("sig=locate-secret"));
    assert!(!structured_debug.contains("token=stream-secret"));
    assert!(!rendered.contains(char::REPLACEMENT_CHARACTER));
    assert!(rendered.lines().all(|line| line.len() < 240));
}

#[test]
fn sanitizer_redacts_credentials_queries_and_control_characters() {
    let unsafe_text = concat!(
        "Cookie=session=top-secret; Authorization: Bearer auth-secret ",
        "https://r.example/audio?expire=1&sig=query-secret\n",
        "set-cookie: sid=response-secret\r\n",
        "proxy_authorization=proxy-secret\tfinished"
    );
    let safe = sanitize(unsafe_text);

    for secret in [
        "top-secret",
        "auth-secret",
        "query-secret",
        "response-secret",
        "proxy-secret",
    ] {
        assert!(!safe.contains(secret));
    }
    assert!(safe.contains("Cookie=[REDACTED]"));
    assert!(safe.contains("Authorization: [REDACTED]"));
    assert!(safe.contains("https://r.example/audio?[REDACTED]"));
    assert!(!safe.contains('\n'));
    assert!(!safe.contains('\r'));
    assert!(!safe.contains('\t'));
}

#[test]
fn sanitizer_handles_case_insensitive_urls_and_space_separated_cookie_flags() {
    let unsafe_text = concat!(
        "HTTPS://media.example/audio?token=upper-query ",
        "--cookies browser-cookie.txt ",
        "--cookies-from-browser firefox ",
        "AUTHORIZATION Bearer header-secret"
    );
    let safe = sanitize(unsafe_text);

    for secret in [
        "upper-query",
        "browser-cookie.txt",
        "firefox",
        "header-secret",
    ] {
        assert!(!safe.contains(secret));
    }
    assert!(safe.contains("HTTPS://media.example/audio?[REDACTED]"));
}

#[test]
fn sanitizer_redacts_cookie_and_authorization_key_variants() {
    let unsafe_text = concat!(
        "Session_Cookie=session-secret ",
        "x-Authorization-Token: header-secret ",
        "cookies-from-browser=browser-secret ",
        "http-cookie-file /tmp/file-secret"
    );
    let safe = sanitize(unsafe_text);

    for secret in [
        "session-secret",
        "header-secret",
        "browser-secret",
        "file-secret",
    ] {
        assert!(!safe.contains(secret));
    }
}

#[test]
fn sanitizer_redacts_quoted_and_json_credentials_without_swallowing_neighbors() {
    let unsafe_text = concat!(
        r#"{"safe":"keep-before","Authorization":"Bearer json-auth-secret","#,
        r#""Cookie":"sid=json-cookie-secret","#,
        r#""url":"HTTPS://media.example/audio?token=json-query-secret","#,
        r#""after":"keep-after"} "#,
        r#"'cOoKiE'='sid=single-cookie-secret' "#,
        r#"'aUtHoRiZaTiOn': 'Bearer single-auth-secret' tail"#
    );
    let safe = sanitize(unsafe_text);

    for secret in [
        "json-auth-secret",
        "json-cookie-secret",
        "json-query-secret",
        "single-cookie-secret",
        "single-auth-secret",
    ] {
        assert!(!safe.contains(secret));
    }
    assert!(safe.contains(r#""safe":"keep-before""#));
    assert!(safe.contains(r#""url":"HTTPS://media.example/audio?[REDACTED]""#));
    assert!(safe.contains(r#""after":"keep-after""#));
    assert!(safe.contains("tail"));
}

#[test]
fn sanitizer_handles_escaped_quotes_and_continues_to_later_sensitive_fields() {
    let unsafe_text = r#"{"Authorization":"Bearer first-\"quoted\"-secret","safe":"middle","Cookie":"sid=later-secret","after":"last"}"#;
    let safe = sanitize(unsafe_text);

    assert!(!safe.contains("first-"));
    assert!(!safe.contains("later-secret"));
    assert!(safe.contains(r#""safe":"middle""#));
    assert!(safe.contains(r#""after":"last""#));
    assert!(safe.matches("[REDACTED]").count() >= 2);
}

#[test]
fn sanitizer_redacts_the_complete_query_when_values_contain_punctuation() {
    let unsafe_text = r#"{"url":"https://media.example/audio?token=first-secret;sig=second-secret,extra=third-secret","after":"kept"}"#;
    let safe = sanitize(unsafe_text);

    for secret in ["first-secret", "second-secret", "third-secret"] {
        assert!(!safe.contains(secret));
    }
    assert!(safe.contains(r#""url":"https://media.example/audio?[REDACTED]""#));
    assert!(safe.contains(r#""after":"kept""#));
}

#[test]
fn sanitizer_redacts_quote_wrapped_headers_without_swallowing_following_text() {
    let unsafe_text = concat!(
        r#""Authorization: Bearer wrapped-secret" after=kept "#,
        r#"'Cookie: sid=single-wrapped-secret' after_single=kept"#
    );
    let safe = sanitize(unsafe_text);

    assert!(!safe.contains("wrapped-secret"));
    assert!(!safe.contains("single-wrapped-secret"));
    assert!(safe.contains(r#""Authorization: [REDACTED]" after=kept"#));
    assert!(safe.contains(r"'Cookie: [REDACTED]' after_single=kept"));
}

#[test]
fn sanitizer_redacts_one_layer_of_escaped_json_credentials() {
    let unsafe_text = r#"{\"Authorization\":\"Bearer escaped-secret\",\"Cookie\":\"sid=later-secret\",\"after\":\"kept\"}"#;
    let safe = sanitize(unsafe_text);

    assert!(!safe.contains("escaped-secret"));
    assert!(!safe.contains("later-secret"));
    assert!(safe.contains(r#"\"Authorization\":\"[REDACTED]\""#));
    assert!(safe.contains(r#"\"Cookie\":\"[REDACTED]\""#));
    assert!(safe.contains(r#"\"after\":\"kept\""#));
}

#[test]
fn sanitizer_redacts_escaped_json_url_without_swallowing_following_fields() {
    let unsafe_text = r#"{\"url\":\"https://m.test/a?token=query-secret\",\"after\":\"kept\"}"#;
    let safe = sanitize(unsafe_text);

    assert!(!safe.contains("query-secret"));
    assert!(safe.contains(r#"\"url\":\"https://m.test/a?[REDACTED]\""#));
    assert!(safe.contains(r#"\"after\":\"kept\""#));
}

#[test]
fn sanitizer_redacts_url_userinfo_and_fragment_secrets() {
    let unsafe_text = concat!(
        "login=https://user:password-secret@host.test/media ",
        "fragment=https://host.test/path#access_token=fragment-secret after=kept"
    );
    let safe = sanitize(unsafe_text);

    assert!(!safe.contains("password-secret"));
    assert!(!safe.contains("fragment-secret"));
    assert!(safe.contains("https://[REDACTED]@host.test/media"));
    assert!(safe.contains("https://host.test/path#[REDACTED]"));
    assert!(safe.contains("after=kept"));
}

#[test]
fn sanitizer_keeps_apostrophes_in_unwrapped_url_paths_and_redacts_the_query() {
    let unsafe_text = "url=https://host.test/a'b?token=apostrophe-secret after=kept";
    let safe = sanitize(unsafe_text);

    assert!(!safe.contains("apostrophe-secret"));
    assert!(safe.contains("https://host.test/a'b?[REDACTED]"));
    assert!(safe.contains("after=kept"));
}

#[test]
fn sanitizer_redacts_multiple_plain_and_escaped_json_urls_without_losing_neighbors() {
    let unsafe_text = concat!(
        r#"{"url":"https://plain-user:plain-secret@plain.test/a?q=query-secret#token=fragment-secret","after":"plain-kept"} "#,
        r#"{\"url\":\"https://escaped.test/b#token=escaped-secret\",\"after\":\"escaped-kept\"} "#,
        "next=https://second.test/c?sig=second-secret tail=kept"
    );
    let safe = sanitize(unsafe_text);

    for secret in [
        "plain-secret",
        "query-secret",
        "fragment-secret",
        "escaped-secret",
        "second-secret",
    ] {
        assert!(!safe.contains(secret));
    }
    assert!(safe.contains(
        r#""url":"https://[REDACTED]@plain.test/a?[REDACTED]#[REDACTED]","after":"plain-kept""#
    ));
    assert!(
        safe.contains(
            r#"\"url\":\"https://escaped.test/b#[REDACTED]\",\"after\":\"escaped-kept\""#
        )
    );
    assert!(safe.contains("next=https://second.test/c?[REDACTED] tail=kept"));
}

#[tokio::test]
async fn cli_doctor_execution_writes_report_and_returns_report_exit_code() {
    let locator = all_dependencies();
    let runner = FakeRunner::new(healthy_outputs());
    let mut buffer = Vec::new();

    let code = match execute_doctor(&locator, &runner, Platform::Windows, &mut buffer).await {
        Ok(code) => code,
        Err(error) => panic!("writing to a byte buffer should succeed: {error}"),
    };
    let rendered = match String::from_utf8(buffer) {
        Ok(rendered) => rendered,
        Err(error) => panic!("report must be UTF-8: {error}"),
    };

    assert_eq!(code, 0);
    assert_eq!(
        rendered,
        DependencyChecker::new(
            &all_dependencies(),
            &FakeRunner::new(healthy_outputs()),
            Platform::Windows,
        )
        .check()
        .await
        .render()
    );
}

#[test]
fn process_debug_output_never_exposes_arguments_or_captured_bytes() {
    let spec = CommandSpec::new(
        Path::new("/path with spaces/tool;name"),
        [
            OsString::from("--authorization"),
            OsString::from("secret-argument"),
        ],
    );
    let spec_debug = format!("{spec:?}");

    assert_eq!(spec.program, PathBuf::from("/path with spaces/tool;name"));
    assert_eq!(
        spec.args,
        [
            OsString::from("--authorization"),
            OsString::from("secret-argument")
        ]
    );
    assert!(!spec_debug.contains("--authorization"));
    assert!(!spec_debug.contains("secret-argument"));
    assert!(spec_debug.contains("arg_count"));
    assert!(spec_debug.contains("limits"));

    let captured = output(
        0,
        b"stdout-secret-marker".to_vec(),
        b"stderr-secret-marker".to_vec(),
    );
    let output_debug = format!("{captured:?}");
    assert!(!output_debug.contains("stdout-secret-marker"));
    assert!(!output_debug.contains("stderr-secret-marker"));
    assert!(!output_debug.contains("stdout: ["));
    assert!(!output_debug.contains("stderr: ["));
    assert!(output_debug.contains("stdout_bytes"));
    assert!(output_debug.contains("stderr_bytes"));
}
