#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "focused integration-test fixtures fail immediately when their setup is invalid"
)]

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::ExitStatus,
    str::FromStr as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::ThreadId,
};

use async_trait::async_trait;
use clap::Parser as _;
use secrecy::{ExposeSecret as _, SecretString};
use ytermusic::{
    auth::{
        AuthError, AuthService, AuthStatus, AuthVerifier, AuthenticatedProviderFactory,
        BlockingSecretStore, Browser, KeyringSecretVault, MAX_COOKIE_HEADER_BYTES,
        MAX_COOKIE_JAR_BYTES, MAX_COOKIE_LINE_BYTES, SecretVault, Verification,
        YtMusicAuthVerifier, parse_netscape_cookie_jar_at,
    },
    cli::{AuthCommand, AuthExecutionError, Cli, Command, execute_auth},
    domain::{ChartSection, MediaId, MediaItem, RegionCode, SearchFilter},
    process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner},
    provider::{
        AuthenticationState, LibraryItem, LibrarySection, MusicProvider, Page, Podcast,
        ProviderError, ProviderErrorKind, ProviderOperation, ProviderResult, SearchItem,
    },
};

const FUTURE_EXPIRY: i64 = 4_102_444_800;
const NOW: i64 = 1_800_000_000;
const SENTINEL: &str = "auth-secret-sentinel-9fc2";

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

fn process_output(code: i32) -> ProcessOutput {
    ProcessOutput {
        status: exit_status(code),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

fn cookie(
    domain: &str,
    include_subdomains: bool,
    path: &str,
    expiry: i64,
    name: &str,
    value: &str,
) -> String {
    format!(
        "{domain}\t{}\t{path}\tTRUE\t{expiry}\t{name}\t{value}\n",
        if include_subdomains { "TRUE" } else { "FALSE" }
    )
}

#[test]
fn parser_keeps_only_live_youtube_music_authentication_cookies() {
    let jar = concat!(
        "# Netscape HTTP Cookie File\n",
        "# an ordinary comment\n",
        "malformed row\n\n",
        ".google.com\tTRUE\t/\tTRUE\t4102444800\tSID\tgoogle\n",
        ".evilyoutube.com\tTRUE\t/\tTRUE\t4102444800\tSID\tlookalike-prefix\n",
        ".youtube.com.evil\tTRUE\t/\tTRUE\t4102444800\tSID\tlookalike-suffix\n",
        "youtube.com\tFALSE\t/\tTRUE\t4102444800\tSID\tparent-host-only\n",
        ".youtube.com\tTRUE\t/\tTRUE\t1799999999\tSSID\texpired\n",
        ".youtube.com\tTRUE\t/private\tTRUE\t4102444800\tHSID\twrong-path\n",
        ".youtube.com\tTRUE\t/\tTRUE\t0\tAPISID\tsession-apisid\n",
        ".youtube.com\tTRUE\t/\tTRUE\t4102444800\tSAPISID\tparent-sapisid\n",
        "music.youtube.com\tFALSE\t/\tTRUE\t4102444800\tSAPISID\tmusic-sapisid\n",
        ".youtube.com\tTRUE\t/\tTRUE\t4102444800\tPREF\tnot-authentication\n",
        ".youtube.com\tTRUE\t/\tTRUE\t4102444800\t__Secure-3PAPISID\tsecure-papisid\n",
        "#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t4102444800\tSID\thttp-only-sid\n",
    );

    let header = parse_netscape_cookie_jar_at(jar.as_bytes(), NOW)
        .expect("the cookie fixture should produce a header");
    let exposed = header.expose_secret();

    assert_eq!(
        exposed,
        "SID=http-only-sid; APISID=session-apisid; SAPISID=music-sapisid; \
         __Secure-3PAPISID=secure-papisid"
    );
    for rejected in [
        "google",
        "lookalike-prefix",
        "lookalike-suffix",
        "parent-host-only",
        "expired",
        "wrong-path",
        "parent-sapisid",
        "not-authentication",
    ] {
        assert!(!exposed.contains(rejected));
    }
}

#[test]
fn parser_rejects_cookie_header_injection_without_disclosing_it() {
    for jar in [
        cookie(
            ".youtube.com",
            true,
            "/",
            FUTURE_EXPIRY,
            "SAPISID",
            &format!("{SENTINEL}; injected=yes"),
        ),
        cookie(
            ".youtube.com",
            true,
            "/",
            FUTURE_EXPIRY,
            "SAPISID",
            &format!("{SENTINEL}\rInjected: yes"),
        ),
        cookie(
            ".youtube.com",
            true,
            "/",
            FUTURE_EXPIRY,
            "SAPISID;Injected",
            SENTINEL,
        ),
    ] {
        let error = parse_netscape_cookie_jar_at(jar.as_bytes(), NOW)
            .expect_err("malicious cookie material must be rejected");
        let debug = format!("{error:?}");
        let display = error.to_string();
        assert!(!debug.contains(SENTINEL));
        assert!(!display.contains(SENTINEL));
    }
}

#[test]
fn parser_enforces_input_line_value_and_header_bounds() {
    let oversized_jar = vec![b'x'; MAX_COOKIE_JAR_BYTES + 1];
    assert!(parse_netscape_cookie_jar_at(&oversized_jar, NOW).is_err());

    let oversized_line = format!(
        "#{}\n{}",
        "x".repeat(MAX_COOKIE_LINE_BYTES),
        cookie(".youtube.com", true, "/", FUTURE_EXPIRY, "SAPISID", "valid")
    );
    assert!(parse_netscape_cookie_jar_at(oversized_line.as_bytes(), NOW).is_err());

    let large_value = "v".repeat((MAX_COOKIE_HEADER_BYTES / 2) + 1);
    let cumulative = [
        cookie(
            ".youtube.com",
            true,
            "/",
            FUTURE_EXPIRY,
            "SID",
            &large_value,
        ),
        cookie(
            ".youtube.com",
            true,
            "/",
            FUTURE_EXPIRY,
            "SAPISID",
            &large_value,
        ),
    ]
    .concat();
    assert!(parse_netscape_cookie_jar_at(cumulative.as_bytes(), NOW).is_err());

    let no_auth_cookie = cookie(".youtube.com", true, "/", FUTURE_EXPIRY, "PREF", "ordinary");
    assert!(parse_netscape_cookie_jar_at(no_auth_cookie.as_bytes(), NOW).is_err());
}

#[test]
fn browser_is_a_closed_set_and_cli_exposes_typed_auth_commands() {
    assert_eq!(Browser::from_str("firefox"), Ok(Browser::Firefox));
    assert_eq!(Browser::Firefox.as_ytdlp_name(), "firefox");
    assert!(Browser::from_str("--config-location=/tmp/evil").is_err());

    let parsed = Cli::try_parse_from(["ytermusic", "auth", "import", "firefox"])
        .expect("supported browser should parse");
    assert!(matches!(
        parsed.command,
        Some(Command::Auth {
            command: AuthCommand::Import {
                browser: Browser::Firefox
            }
        })
    ));
    assert!(
        Cli::try_parse_from(["ytermusic", "auth", "import", "--config-location=/tmp/evil"])
            .is_err()
    );
}

#[derive(Clone)]
enum RunnerBehavior {
    Export(&'static [u8]),
    NonzeroExport(&'static [u8]),
    ExitFailure,
    ProcessFailure,
    Panic,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedCall {
    program: PathBuf,
    args: Vec<OsString>,
    path_existed_during_call: bool,
    #[cfg(unix)]
    mode_during_call: u32,
}

#[derive(Default)]
struct TempTracker {
    path: Mutex<Option<PathBuf>>,
}

impl TempTracker {
    fn set(&self, path: PathBuf) {
        *self
            .path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path);
    }

    fn path(&self) -> Option<PathBuf> {
        self.path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn is_absent(&self) -> bool {
        self.path().is_some_and(|path| !path.exists())
    }
}

struct FakeRunner {
    behavior: RunnerBehavior,
    calls: Mutex<Vec<RecordedCall>>,
    tracker: Arc<TempTracker>,
}

impl FakeRunner {
    fn new(behavior: RunnerBehavior, tracker: Arc<TempTracker>) -> Self {
        Self {
            behavior,
            calls: Mutex::new(Vec::new()),
            tracker,
        }
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl ProcessRunner for FakeRunner {
    async fn output(&self, spec: CommandSpec) -> Result<ProcessOutput, ProcessError> {
        let path = PathBuf::from(
            spec.args
                .get(3)
                .and_then(|value| value.to_str())
                .expect("auth command must provide a UTF-8 temporary path"),
        );
        self.tracker.set(path.clone());
        let metadata = fs::metadata(&path).expect("temporary file must exist before launch");
        let seeded = fs::read(&path).expect("temporary file seed should be readable");
        assert_eq!(
            seeded, b"# Netscape HTTP Cookie File\n",
            "yt-dlp must receive a valid, flushed Netscape jar"
        );
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt as _;
            metadata.permissions().mode() & 0o777
        };
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedCall {
                program: spec.program.clone(),
                args: spec.args.clone(),
                path_existed_during_call: metadata.is_file(),
                #[cfg(unix)]
                mode_during_call: mode,
            });
        match self.behavior.clone() {
            RunnerBehavior::Export(contents) => {
                fs::write(&path, contents).expect("fake yt-dlp export should be writable");
                Ok(process_output(0))
            }
            RunnerBehavior::NonzeroExport(contents) => {
                fs::write(&path, contents).expect("fake yt-dlp export should be writable");
                Ok(ProcessOutput {
                    status: exit_status(1),
                    stdout: SENTINEL.as_bytes().to_vec(),
                    stderr: SENTINEL.as_bytes().to_vec(),
                })
            }
            RunnerBehavior::ExitFailure => Ok(ProcessOutput {
                status: exit_status(1),
                stdout: SENTINEL.as_bytes().to_vec(),
                stderr: SENTINEL.as_bytes().to_vec(),
            }),
            RunnerBehavior::ProcessFailure => Err(ProcessError::Spawn {
                program: spec.program,
                source: io::Error::new(io::ErrorKind::NotFound, SENTINEL),
            }),
            RunnerBehavior::Panic => panic!("fake runner panic"),
        }
    }
}

#[derive(Clone, Copy)]
enum VerifierBehavior {
    Connected,
    Expired,
    Unavailable,
}

struct FakeVerifier {
    behavior: VerifierBehavior,
    calls: AtomicUsize,
    tracker: Arc<TempTracker>,
    saw_expected_cookie: AtomicBool,
}

impl FakeVerifier {
    fn new(behavior: VerifierBehavior, tracker: Arc<TempTracker>) -> Self {
        Self {
            behavior,
            calls: AtomicUsize::new(0),
            tracker,
            saw_expected_cookie: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl AuthVerifier for FakeVerifier {
    async fn verify(&self, cookie: &SecretString) -> Result<Verification, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.tracker.path().is_none() || self.tracker.is_absent(),
            "the export must be deleted before verification"
        );
        self.saw_expected_cookie.store(
            cookie.expose_secret().contains("SAPISID=imported"),
            Ordering::SeqCst,
        );
        match self.behavior {
            VerifierBehavior::Connected => Ok(Verification::Connected),
            VerifierBehavior::Expired => Ok(Verification::Expired),
            VerifierBehavior::Unavailable => Err(AuthError::VerificationUnavailable),
        }
    }
}

struct FakeVault {
    cookie: Mutex<Option<SecretString>>,
    fail_load: AtomicBool,
    fail_store: AtomicBool,
    fail_delete: AtomicBool,
    store_calls: AtomicUsize,
    delete_calls: AtomicUsize,
    tracker: Arc<TempTracker>,
}

impl FakeVault {
    fn new(cookie: Option<&str>, tracker: Arc<TempTracker>) -> Self {
        Self {
            cookie: Mutex::new(cookie.map(SecretString::from)),
            fail_load: AtomicBool::new(false),
            fail_store: AtomicBool::new(false),
            fail_delete: AtomicBool::new(false),
            store_calls: AtomicUsize::new(0),
            delete_calls: AtomicUsize::new(0),
            tracker,
        }
    }

    fn exposed_cookie(&self) -> Option<String> {
        self.cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|cookie| cookie.expose_secret().to_owned())
    }
}

#[async_trait]
impl SecretVault for FakeVault {
    async fn load_cookie(&self) -> Result<Option<SecretString>, AuthError> {
        if self.fail_load.load(Ordering::SeqCst) {
            return Err(AuthError::VaultUnavailable);
        }
        Ok(self
            .cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    async fn store_cookie(&self, cookie: SecretString) -> Result<(), AuthError> {
        self.store_calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.tracker.path().is_none() || self.tracker.is_absent(),
            "the export must be deleted before vault storage"
        );
        if self.fail_store.load(Ordering::SeqCst) {
            return Err(AuthError::VaultUnavailable);
        }
        *self
            .cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cookie);
        Ok(())
    }

    async fn delete_cookie(&self) -> Result<(), AuthError> {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(AuthError::VaultUnavailable);
        }
        *self
            .cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

fn valid_export() -> &'static [u8] {
    b"# Netscape HTTP Cookie File\n\
      .youtube.com\tTRUE\t/\tTRUE\t4102444800\tSAPISID\timported\n\
      .youtube.com\tTRUE\t/\tTRUE\t4102444800\t__Secure-3PAPISID\tsecure\n"
}

fn service(
    vault: Arc<FakeVault>,
    runner: Arc<FakeRunner>,
    verifier: Arc<FakeVerifier>,
) -> AuthService {
    AuthService::new(vault, runner, verifier)
}

#[tokio::test]
async fn import_uses_exact_safe_argv_private_file_and_stores_only_after_cleanup() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("old-cookie"), Arc::clone(&tracker)));
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::Export(valid_export()),
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Connected,
        Arc::clone(&tracker),
    ));
    let auth = service(
        Arc::clone(&vault),
        Arc::clone(&runner),
        Arc::clone(&verifier),
    );

    auth.import_browser(Browser::Firefox)
        .await
        .expect("valid import should succeed");

    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.program, Path::new("yt-dlp"));
    assert_eq!(
        &call.args[..3],
        [
            OsString::from("--cookies-from-browser"),
            OsString::from("firefox"),
            OsString::from("--cookies"),
        ]
    );
    assert_eq!(
        call.args.get(3).map(PathBuf::from),
        tracker.path(),
        "the fourth and final argument is the pre-created export path"
    );
    assert_eq!(call.args.len(), 4);
    assert!(call.path_existed_during_call);
    #[cfg(unix)]
    assert_eq!(call.mode_during_call, 0o600);
    assert!(tracker.is_absent());
    assert!(verifier.saw_expected_cookie.load(Ordering::SeqCst));
    assert_eq!(vault.store_calls.load(Ordering::SeqCst), 1);
    let stored = vault
        .exposed_cookie()
        .expect("successful import should replace the old cookie");
    assert!(stored.contains("SAPISID=imported"));
    assert!(!stored.contains("old-cookie"));
}

#[tokio::test]
async fn nonzero_export_with_a_valid_jar_still_verifies_and_stores() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("old-cookie"), Arc::clone(&tracker)));
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::NonzeroExport(valid_export()),
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Connected,
        Arc::clone(&tracker),
    ));
    let auth = service(Arc::clone(&vault), runner, Arc::clone(&verifier));

    auth.import_browser(Browser::Chrome)
        .await
        .expect("a valid exported jar should survive yt-dlp's no-URL exit");

    assert!(tracker.is_absent());
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 1);
    assert_eq!(vault.store_calls.load(Ordering::SeqCst), 1);
    assert!(
        vault
            .exposed_cookie()
            .is_some_and(|cookie| cookie.contains("SAPISID=imported"))
    );
}

#[tokio::test]
async fn nonzero_export_with_only_the_seed_header_is_an_export_failure() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("old-cookie"), Arc::clone(&tracker)));
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::ExitFailure,
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Connected,
        Arc::clone(&tracker),
    ));
    let auth = service(Arc::clone(&vault), runner, Arc::clone(&verifier));

    let error = auth
        .import_browser(Browser::Chrome)
        .await
        .expect_err("seed-only nonzero export must fail");

    assert_eq!(error, AuthError::ExportFailed);
    assert!(!format!("{error:?}").contains(SENTINEL));
    assert!(!error.to_string().contains(SENTINEL));
    assert!(tracker.is_absent());
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    assert_eq!(vault.store_calls.load(Ordering::SeqCst), 0);
    assert_eq!(vault.exposed_cookie().as_deref(), Some("old-cookie"));
}

#[tokio::test]
async fn execute_auth_import_runs_the_service_and_prints_only_connected() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(None, Arc::clone(&tracker)));
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::Export(valid_export()),
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Connected,
        Arc::clone(&tracker),
    ));
    let auth = service(Arc::clone(&vault), runner, verifier);
    let mut output = Vec::new();

    execute_auth(
        AuthCommand::Import {
            browser: Browser::Firefox,
        },
        &auth,
        &mut output,
    )
    .await
    .expect("valid import command should succeed");

    assert_eq!(output, b"connected\n");
    assert_eq!(vault.store_calls.load(Ordering::SeqCst), 1);
    assert!(!String::from_utf8_lossy(&output).contains("SAPISID"));
}

#[tokio::test]
async fn execute_auth_status_prints_exact_safe_lines_for_every_status() {
    for (cookie, behavior, expected) in [
        (None, VerifierBehavior::Connected, b"anonymous\n".as_slice()),
        (
            Some("SAPISID=stored; SID=value"),
            VerifierBehavior::Connected,
            b"connected\n".as_slice(),
        ),
        (
            Some("SAPISID=expired; SID=value"),
            VerifierBehavior::Expired,
            b"expired\n".as_slice(),
        ),
    ] {
        let tracker = Arc::new(TempTracker::default());
        let vault = Arc::new(FakeVault::new(cookie, Arc::clone(&tracker)));
        let runner = Arc::new(FakeRunner::new(
            RunnerBehavior::ProcessFailure,
            Arc::clone(&tracker),
        ));
        let verifier = Arc::new(FakeVerifier::new(behavior, Arc::clone(&tracker)));
        let auth = service(vault, runner, verifier);
        let mut output = Vec::new();

        execute_auth(AuthCommand::Status, &auth, &mut output)
            .await
            .expect("status command should succeed");

        assert_eq!(output, expected);
        assert!(!String::from_utf8_lossy(&output).contains("SAPISID"));
    }
}

#[tokio::test]
async fn execute_auth_logout_deletes_and_confirms_without_identity() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("SAPISID=stored"), Arc::clone(&tracker)));
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::ProcessFailure,
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Unavailable,
        Arc::clone(&tracker),
    ));
    let auth = service(Arc::clone(&vault), runner, verifier);
    let mut output = Vec::new();

    execute_auth(AuthCommand::Logout, &auth, &mut output)
        .await
        .expect("logout command should succeed");

    assert_eq!(output, b"logged out\n");
    assert_eq!(vault.exposed_cookie(), None);
    assert!(!String::from_utf8_lossy(&output).contains("SAPISID"));
}

#[tokio::test]
async fn execute_auth_propagates_auth_failures_without_printing_success() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("SAPISID=stored"), Arc::clone(&tracker)));
    vault.fail_load.store(true, Ordering::SeqCst);
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::ProcessFailure,
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Connected,
        Arc::clone(&tracker),
    ));
    let auth = service(vault, runner, verifier);
    let mut output = Vec::new();

    let result = execute_auth(AuthCommand::Status, &auth, &mut output).await;

    assert_eq!(
        result,
        Err(AuthExecutionError::Authentication(
            AuthError::VaultUnavailable
        ))
    );
    assert!(output.is_empty());
}

#[tokio::test]
async fn failed_verification_never_overwrites_the_existing_vault_cookie() {
    for behavior in [VerifierBehavior::Expired, VerifierBehavior::Unavailable] {
        let tracker = Arc::new(TempTracker::default());
        let vault = Arc::new(FakeVault::new(Some("old-cookie"), Arc::clone(&tracker)));
        let runner = Arc::new(FakeRunner::new(
            RunnerBehavior::Export(valid_export()),
            Arc::clone(&tracker),
        ));
        let verifier = Arc::new(FakeVerifier::new(behavior, Arc::clone(&tracker)));
        let auth = service(Arc::clone(&vault), runner, verifier);

        assert!(auth.import_browser(Browser::Chrome).await.is_err());
        assert!(tracker.is_absent());
        assert_eq!(vault.store_calls.load(Ordering::SeqCst), 0);
        assert_eq!(vault.exposed_cookie().as_deref(), Some("old-cookie"));
    }
}

#[tokio::test]
async fn tempfile_is_removed_after_runner_parser_and_vault_failures() {
    for behavior in [
        RunnerBehavior::ExitFailure,
        RunnerBehavior::ProcessFailure,
        RunnerBehavior::Export(b"not a cookie jar"),
        RunnerBehavior::Panic,
    ] {
        let tracker = Arc::new(TempTracker::default());
        let vault = Arc::new(FakeVault::new(None, Arc::clone(&tracker)));
        let runner = Arc::new(FakeRunner::new(behavior, Arc::clone(&tracker)));
        let verifier = Arc::new(FakeVerifier::new(
            VerifierBehavior::Connected,
            Arc::clone(&tracker),
        ));
        let auth = service(vault, runner, verifier);

        let error = auth
            .import_browser(Browser::Safari)
            .await
            .expect_err("the configured failure must be returned");
        assert!(tracker.is_absent());
        assert!(!format!("{error:?}").contains(SENTINEL));
        assert!(!error.to_string().contains(SENTINEL));
    }

    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("old-cookie"), Arc::clone(&tracker)));
    vault.fail_store.store(true, Ordering::SeqCst);
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::Export(valid_export()),
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Connected,
        Arc::clone(&tracker),
    ));
    let auth = service(Arc::clone(&vault), runner, verifier);
    assert!(auth.import_browser(Browser::Edge).await.is_err());
    assert!(tracker.is_absent());
    assert_eq!(vault.exposed_cookie().as_deref(), Some("old-cookie"));
}

#[tokio::test]
async fn status_distinguishes_anonymous_expired_connected_and_transient_failure() {
    for (cookie, behavior, expected) in [
        (None, VerifierBehavior::Connected, AuthStatus::Anonymous),
        (
            Some("SAPISID=stored; SID=value"),
            VerifierBehavior::Connected,
            AuthStatus::Connected,
        ),
        (
            Some("SAPISID=expired; SID=value"),
            VerifierBehavior::Expired,
            AuthStatus::Expired,
        ),
    ] {
        let tracker = Arc::new(TempTracker::default());
        let vault = Arc::new(FakeVault::new(cookie, Arc::clone(&tracker)));
        let runner = Arc::new(FakeRunner::new(
            RunnerBehavior::ProcessFailure,
            Arc::clone(&tracker),
        ));
        let verifier = Arc::new(FakeVerifier::new(behavior, Arc::clone(&tracker)));
        let auth = service(vault, runner, Arc::clone(&verifier));

        assert_eq!(auth.status().await, Ok(expected));
        assert_eq!(
            verifier.calls.load(Ordering::SeqCst),
            usize::from(cookie.is_some())
        );
    }

    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(
        Some("SAPISID=stored; SID=value"),
        Arc::clone(&tracker),
    ));
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::ProcessFailure,
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Unavailable,
        Arc::clone(&tracker),
    ));
    let auth = service(vault, runner, verifier);
    assert_eq!(auth.status().await, Err(AuthError::VerificationUnavailable));
}

#[tokio::test]
async fn status_propagates_vault_load_failure_without_calling_the_canary() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("SAPISID=stored"), Arc::clone(&tracker)));
    vault.fail_load.store(true, Ordering::SeqCst);
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::ProcessFailure,
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Connected,
        Arc::clone(&tracker),
    ));
    let auth = service(vault, Arc::clone(&runner), Arc::clone(&verifier));

    assert_eq!(auth.status().await, Err(AuthError::VaultUnavailable));
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn logout_deletes_only_the_ytermusic_vault_entry_without_other_boundaries() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("SAPISID=stored"), Arc::clone(&tracker)));
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::ProcessFailure,
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Unavailable,
        Arc::clone(&tracker),
    ));
    let auth = service(
        Arc::clone(&vault),
        Arc::clone(&runner),
        Arc::clone(&verifier),
    );

    auth.logout().await.expect("logout should delete the entry");

    assert_eq!(vault.exposed_cookie(), None);
    assert_eq!(vault.delete_calls.load(Ordering::SeqCst), 1);
    assert!(runner.calls().is_empty());
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn logout_propagates_vault_delete_failure_without_other_side_effects() {
    let tracker = Arc::new(TempTracker::default());
    let vault = Arc::new(FakeVault::new(Some("SAPISID=stored"), Arc::clone(&tracker)));
    vault.fail_delete.store(true, Ordering::SeqCst);
    let runner = Arc::new(FakeRunner::new(
        RunnerBehavior::ProcessFailure,
        Arc::clone(&tracker),
    ));
    let verifier = Arc::new(FakeVerifier::new(
        VerifierBehavior::Unavailable,
        Arc::clone(&tracker),
    ));
    let auth = service(
        Arc::clone(&vault),
        Arc::clone(&runner),
        Arc::clone(&verifier),
    );

    assert_eq!(auth.logout().await, Err(AuthError::VaultUnavailable));
    assert_eq!(vault.delete_calls.load(Ordering::SeqCst), 1);
    assert_eq!(vault.exposed_cookie().as_deref(), Some("SAPISID=stored"));
    assert!(runner.calls().is_empty());
    assert_eq!(verifier.calls.load(Ordering::SeqCst), 0);
}

#[derive(Default)]
struct ThreadRecordingStore {
    cookie: Mutex<Option<SecretString>>,
    threads: Mutex<Vec<ThreadId>>,
}

impl ThreadRecordingStore {
    fn record(&self) {
        self.threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(std::thread::current().id());
    }
}

impl BlockingSecretStore for ThreadRecordingStore {
    fn load_cookie(&self) -> Result<Option<SecretString>, AuthError> {
        self.record();
        Ok(self
            .cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn store_cookie(&self, cookie: SecretString) -> Result<(), AuthError> {
        self.record();
        *self
            .cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cookie);
        Ok(())
    }

    fn delete_cookie(&self) -> Result<(), AuthError> {
        self.record();
        *self
            .cookie
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        Ok(())
    }
}

struct FakeCanaryProvider {
    result: Result<(), ProviderError>,
    library_calls: Mutex<Vec<LibrarySection>>,
}

impl FakeCanaryProvider {
    fn new(error_kind: Option<ProviderErrorKind>) -> Self {
        Self {
            result: error_kind.map_or(Ok(()), |kind| {
                Err(ProviderError::new(ProviderOperation::Library, kind))
            }),
            library_calls: Mutex::new(Vec::new()),
        }
    }

    fn library_calls(&self) -> Vec<LibrarySection> {
        self.library_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl MusicProvider for FakeCanaryProvider {
    async fn search(
        &self,
        _query: &str,
        _filter: SearchFilter,
    ) -> ProviderResult<Page<SearchItem>> {
        panic!("authentication canary must not search")
    }

    async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        panic!("authentication canary must not request charts")
    }

    async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
        panic!("authentication canary must not request a playlist")
    }

    async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
        panic!("authentication canary must not request a podcast")
    }

    async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        panic!("authentication canary must not request radio")
    }

    async fn library(&self, section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        self.library_calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(section);
        self.result?;
        Ok(Page {
            items: Vec::new(),
            continuation: None,
            stale: false,
        })
    }

    fn authentication(&self) -> AuthenticationState {
        AuthenticationState::Authenticated
    }
}

struct FakeAuthenticatedProviderFactory {
    provider: Arc<FakeCanaryProvider>,
    create_error: Option<ProviderError>,
    calls: AtomicUsize,
    saw_secret: AtomicBool,
}

impl FakeAuthenticatedProviderFactory {
    fn new(provider: Arc<FakeCanaryProvider>, create_error: Option<ProviderError>) -> Self {
        Self {
            provider,
            create_error,
            calls: AtomicUsize::new(0),
            saw_secret: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl AuthenticatedProviderFactory for FakeAuthenticatedProviderFactory {
    async fn create(&self, cookie: &SecretString) -> ProviderResult<Arc<dyn MusicProvider>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.saw_secret
            .store(cookie.expose_secret().contains(SENTINEL), Ordering::SeqCst);
        if let Some(error) = self.create_error {
            return Err(error);
        }
        let provider: Arc<dyn MusicProvider> = self.provider.clone();
        Ok(provider)
    }
}

#[tokio::test]
async fn production_verifier_calls_authenticated_playlist_canary_and_classifies_it() {
    for (provider_error, expected) in [
        (None, Ok(Verification::Connected)),
        (
            Some(ProviderErrorKind::AuthenticationRequired),
            Ok(Verification::Expired),
        ),
        (
            Some(ProviderErrorKind::Unavailable),
            Err(AuthError::VerificationUnavailable),
        ),
    ] {
        let provider = Arc::new(FakeCanaryProvider::new(provider_error));
        let factory = Arc::new(FakeAuthenticatedProviderFactory::new(
            Arc::clone(&provider),
            None,
        ));
        let verifier = YtMusicAuthVerifier::with_factory(factory.clone());
        let cookie = SecretString::from(format!("SAPISID={SENTINEL}; SID=value"));

        assert_eq!(verifier.verify(&cookie).await, expected);
        assert_eq!(provider.library_calls(), [LibrarySection::Playlists]);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
        assert!(factory.saw_secret.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn production_verifier_classifies_provider_construction_failures_without_a_canary() {
    for (kind, expected) in [
        (
            ProviderErrorKind::AuthenticationRequired,
            Ok(Verification::Expired),
        ),
        (
            ProviderErrorKind::Unavailable,
            Err(AuthError::VerificationUnavailable),
        ),
    ] {
        let provider = Arc::new(FakeCanaryProvider::new(None));
        let factory = Arc::new(FakeAuthenticatedProviderFactory::new(
            Arc::clone(&provider),
            Some(ProviderError::new(ProviderOperation::Authentication, kind)),
        ));
        let verifier = YtMusicAuthVerifier::with_factory(factory);

        assert_eq!(
            verifier.verify(&SecretString::from(SENTINEL)).await,
            expected
        );
        assert!(provider.library_calls().is_empty());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn keyring_facade_moves_every_blocking_store_operation_off_the_runtime_thread() {
    let runtime_thread = std::thread::current().id();
    let store = Arc::new(ThreadRecordingStore::default());
    let vault = KeyringSecretVault::with_store(store.clone());

    vault
        .store_cookie(SecretString::from(SENTINEL))
        .await
        .expect("store should succeed");
    let loaded = vault
        .load_cookie()
        .await
        .expect("load should succeed")
        .expect("stored credential should exist");
    assert_eq!(loaded.expose_secret(), SENTINEL);
    vault.delete_cookie().await.expect("delete should succeed");

    let threads = store
        .threads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(threads.len(), 3);
    assert!(threads.iter().all(|thread| *thread != runtime_thread));
}

#[test]
fn secret_bearing_types_and_errors_redact_debug_and_display() {
    let secret = SecretString::from(SENTINEL);
    let debug = format!("{secret:?}");
    assert!(!debug.contains(SENTINEL));

    for error in [
        AuthError::VaultUnavailable,
        AuthError::ExportUnavailable,
        AuthError::ExportFailed,
        AuthError::InvalidCookieJar,
        AuthError::NoAuthenticationCookies,
        AuthError::VerificationRejected,
        AuthError::VerificationUnavailable,
        AuthError::CleanupFailed,
    ] {
        assert!(!format!("{error:?}").contains(SENTINEL));
        assert!(!error.to_string().contains(SENTINEL));
    }
}
