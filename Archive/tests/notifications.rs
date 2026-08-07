use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    io::Cursor,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use ytermusic::{
    app::Generation,
    domain::{MediaId, MediaItem, MediaKind},
    notifications::{
        ArtworkTransportError, BoundedNotificationArtworkLoader,
        CommittedNativeNotificationBackend, NativeArtworkMode, NativeNotificationBackend,
        NativeNotificationRequest, NativeNotificationSubmitter, NativeNotifier,
        NativeSubmissionRequest, NotificationArtworkCache, NotificationArtworkLoader,
        NotificationArtworkStream, NotificationArtworkTransport, NotificationWorker,
        NowPlayingNotification, PrivatePngAttachment, RuntimeNotifier, RuntimeNotifierError,
        initialize_notification_service, linux_replacement_id, run_owned_blocking,
        windows_artwork_mode, windows_notifications_supported,
    },
};

#[tokio::test(flavor = "current_thread")]
async fn notification_cache_startup_is_bounded_off_runtime_and_falls_back_to_text_only() {
    let started = std::time::Instant::now();
    let service = initialize_notification_service(
        true,
        Duration::from_millis(20),
        || {
            std::thread::sleep(Duration::from_millis(200));
            Err(RuntimeNotifierError::unavailable())
        },
        |cache| Ok(cache.is_none()),
    )
    .await;

    assert_eq!(service, Some(true));
    assert!(
        started.elapsed() < Duration::from_millis(120),
        "startup waited for blocking cache preparation"
    );
}

#[tokio::test]
async fn disabled_notifications_still_prune_cache_without_constructing_a_service()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let directory = root.path().join("notifications");
    std::fs::create_dir_all(&directory)?;
    let stale = directory.join("now-playing-1-0.png");
    std::fs::write(&stale, b"stale")?;
    let prepare_root = root.path().to_owned();
    let constructed = Arc::new(AtomicUsize::new(0));
    let construct_count = Arc::clone(&constructed);

    let service: Option<()> = initialize_notification_service(
        false,
        Duration::from_secs(1),
        move || NotificationArtworkCache::new(&prepare_root),
        move |_| {
            construct_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;

    assert!(service.is_none());
    assert!(!stale.exists());
    assert_eq!(constructed.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn unavailable_notification_service_still_prunes_cache_and_returns_no_service()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let directory = root.path().join("notifications");
    std::fs::create_dir_all(&directory)?;
    let stale = directory.join("now-playing-2-0.png");
    std::fs::write(&stale, b"stale")?;
    let prepare_root = root.path().to_owned();
    let constructed = Arc::new(AtomicUsize::new(0));
    let construct_count = Arc::clone(&constructed);

    let service: Option<()> = initialize_notification_service(
        true,
        Duration::from_secs(1),
        move || NotificationArtworkCache::new(&prepare_root),
        move |_| {
            construct_count.fetch_add(1, Ordering::SeqCst);
            Err(RuntimeNotifierError::unavailable())
        },
    )
    .await;

    assert!(service.is_none());
    assert!(!stale.exists());
    assert_eq!(constructed.load(Ordering::SeqCst), 1);
    Ok(())
}

#[cfg(unix)]
#[test]
fn notification_cache_rejects_a_final_symlink_without_touching_its_target()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = tempfile::tempdir()?;
    let foreign = tempfile::tempdir()?;
    let protected = foreign.path().join("do-not-touch.txt");
    std::fs::write(&protected, b"foreign")?;
    std::fs::set_permissions(foreign.path(), std::fs::Permissions::from_mode(0o755))?;
    symlink(foreign.path(), root.path().join("notifications"))?;

    let result = NotificationArtworkCache::new(root.path());

    assert!(result.is_err());
    assert_eq!(std::fs::read(&protected)?, b"foreign");
    assert_eq!(
        std::fs::metadata(foreign.path())?.permissions().mode() & 0o777,
        0o755
    );
    assert!(
        std::fs::symlink_metadata(root.path().join("notifications"))?
            .file_type()
            .is_symlink()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn disabled_notification_startup_rejects_a_final_symlink_without_touching_its_target()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir()?;
    let foreign = tempfile::tempdir()?;
    let protected = foreign.path().join("disabled-do-not-touch.txt");
    std::fs::write(&protected, b"foreign")?;
    symlink(foreign.path(), root.path().join("notifications"))?;
    let prepare_root = root.path().to_owned();
    let constructed = Arc::new(AtomicUsize::new(0));
    let construct_count = Arc::clone(&constructed);

    let service: Option<()> = initialize_notification_service(
        false,
        Duration::from_secs(1),
        move || NotificationArtworkCache::new(&prepare_root),
        move |_| {
            construct_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    )
    .await;

    assert!(service.is_none());
    assert_eq!(std::fs::read(&protected)?, b"foreign");
    assert_eq!(constructed.load(Ordering::SeqCst), 0);
    Ok(())
}

#[cfg(unix)]
#[test]
fn notification_cache_prunes_only_exact_owned_regular_png_files()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::{
        fs::{FileTypeExt as _, symlink},
        net::UnixListener,
    };

    let root = tempfile::tempdir()?;
    let directory = root.path().join("notifications");
    std::fs::create_dir(&directory)?;
    for owned in [
        "now-playing-0-0.png",
        "now-playing-7-42.png",
        "now-playing-18446744073709551615-18446744073709551615.png",
    ] {
        std::fs::write(directory.join(owned), b"owned")?;
    }
    let foreign_names = [
        "foreign.png",
        "now-playing-1.png",
        "now-playing-1-2.PNG",
        "now-playing-01-2.png",
        "now-playing-1-02.png",
        "now-playing-1-2.png.bak",
        "xnow-playing-1-2.png",
        "now-playing-1--2.png",
        "now-playing-18446744073709551616-1.png",
    ];
    for foreign in foreign_names {
        std::fs::write(directory.join(foreign), b"foreign")?;
    }
    let foreign_dir = directory.join("now-playing-8-9.png");
    std::fs::create_dir(&foreign_dir)?;
    std::fs::write(foreign_dir.join("nested.txt"), b"nested")?;
    let symlink_target = root.path().join("symlink-target.txt");
    std::fs::write(&symlink_target, b"target")?;
    let symlink_entry = directory.join("now-playing-10-11.png");
    symlink(&symlink_target, &symlink_entry)?;
    let socket_path = directory.join("now-playing-12-13.png");
    let _socket = UnixListener::bind(&socket_path)?;

    let _cache = NotificationArtworkCache::new(root.path())?;

    for owned in [
        "now-playing-0-0.png",
        "now-playing-7-42.png",
        "now-playing-18446744073709551615-18446744073709551615.png",
    ] {
        assert!(
            !directory.join(owned).exists(),
            "owned file survived: {owned}"
        );
    }
    for foreign in foreign_names {
        assert!(
            directory.join(foreign).exists(),
            "foreign file removed: {foreign}"
        );
    }
    assert_eq!(std::fs::read(foreign_dir.join("nested.txt"))?, b"nested");
    assert!(
        std::fs::symlink_metadata(&symlink_entry)?
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read(&symlink_target)?, b"target");
    assert!(
        std::fs::symlink_metadata(&socket_path)?
            .file_type()
            .is_socket()
    );
    Ok(())
}

fn media(title: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "secret-provider".to_owned(),
            video_id: "secret-video-id".to_owned(),
        },
        kind: MediaKind::Song,
        title: title.to_owned(),
        creators: vec!["Artist 🎶".repeat(200)],
        collection: Some("Album 💿".repeat(200)),
        duration_ms: Some(120_000),
        artwork_url: Some(
            "https://example.invalid/private.png?token=secret"
                .parse()
                .unwrap_or_else(|error| panic!("valid test URL: {error}")),
        ),
        explicit: false,
    }
}

enum FakeTransportMode {
    FetchError,
    StatusError,
    StreamError,
    Oversize,
    CancelDuringStream(CancellationToken),
}

struct FakeArtworkTransport {
    calls: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    mode: FakeTransportMode,
}

#[async_trait]
impl NotificationArtworkTransport for FakeArtworkTransport {
    async fn fetch(
        &self,
        _url: &url::Url,
        _cancel: &CancellationToken,
    ) -> Result<NotificationArtworkStream, ArtworkTransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.mode {
            FakeTransportMode::FetchError | FakeTransportMode::StatusError => {
                Err(ArtworkTransportError::unavailable())
            }
            FakeTransportMode::StreamError => Ok(Box::pin(stream::once(async {
                Err(ArtworkTransportError::unavailable())
            }))),
            FakeTransportMode::Oversize => Ok(Box::pin(stream::once(async {
                Ok(Bytes::from(vec![0; 4 * 1024 * 1024 + 1]))
            }))),
            FakeTransportMode::CancelDuringStream(cancel) => {
                let cancel = cancel.clone();
                let polls = Arc::clone(&self.polls);
                Ok(Box::pin(stream::poll_fn(move |_| {
                    let observed = polls.fetch_add(1, Ordering::SeqCst);
                    if observed == 0 {
                        cancel.cancel();
                        std::task::Poll::Ready(Some(Ok(Bytes::from_static(b"first"))))
                    } else {
                        std::task::Poll::Ready(Some(Ok(Bytes::from_static(b"stale"))))
                    }
                })))
            }
        }
    }
}

fn fake_artwork_url() -> ytermusic::domain::ArtworkUrl {
    ytermusic::domain::ArtworkUrl::try_from(
        "https://example.invalid/art.png?token=secret"
            .parse::<url::Url>()
            .unwrap_or_else(|error| panic!("valid test URL: {error}")),
    )
    .unwrap_or_else(|error| panic!("valid artwork URL: {error}"))
}

#[tokio::test]
async fn bounded_loader_pre_cancel_never_polls_transport() {
    let calls = Arc::new(AtomicUsize::new(0));
    let loader = BoundedNotificationArtworkLoader::new(Arc::new(FakeArtworkTransport {
        calls: Arc::clone(&calls),
        polls: Arc::new(AtomicUsize::new(0)),
        mode: FakeTransportMode::FetchError,
    }));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        loader
            .load(Some(&fake_artwork_url()), &cancel)
            .await
            .is_none()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn bounded_loader_cancellation_during_stream_stops_before_next_poll() {
    let cancel = CancellationToken::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let loader = BoundedNotificationArtworkLoader::new(Arc::new(FakeArtworkTransport {
        calls: Arc::new(AtomicUsize::new(0)),
        polls: Arc::clone(&polls),
        mode: FakeTransportMode::CancelDuringStream(cancel.clone()),
    }));
    assert!(
        loader
            .load(Some(&fake_artwork_url()), &cancel)
            .await
            .is_none()
    );
    assert_eq!(polls.load(Ordering::SeqCst), 1);
}

async fn assert_transport_failure_falls_back_to_text(
    mode: FakeTransportMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let loader = BoundedNotificationArtworkLoader::new(Arc::new(FakeArtworkTransport {
        calls: Arc::new(AtomicUsize::new(0)),
        polls: Arc::new(AtomicUsize::new(0)),
        mode,
    }));
    let (sent, received) = std::sync::mpsc::channel();
    let backend_calls = Arc::new(AtomicUsize::new(0));
    let notifier = NativeNotifier::with_services(
        Arc::new(loader),
        Arc::new(RequestRecordingBackend {
            sent: Mutex::new(Some(sent)),
            fail: false,
            calls: Arc::clone(&backend_calls),
        }),
    );
    notifier
        .notify(
            NowPlayingNotification::from_media(Generation::new(15), &media("Fallback")),
            CancellationToken::new(),
        )
        .await?;
    let request = received.recv()?;
    assert_eq!(request.title(), "Fallback");
    assert!(request.body().contains("Artist"));
    assert!(request.artwork_path().is_none());
    assert_eq!(backend_calls.load(Ordering::SeqCst), 1);
    let rendered = format!("{request:?}");
    assert!(!rendered.contains("example.invalid"));
    assert!(!rendered.contains("token=secret"));
    Ok(())
}

#[tokio::test]
async fn transport_and_oversize_failures_each_send_text_only_without_native_ui()
-> Result<(), Box<dyn std::error::Error>> {
    assert_transport_failure_falls_back_to_text(FakeTransportMode::FetchError).await?;
    assert_transport_failure_falls_back_to_text(FakeTransportMode::StatusError).await?;
    assert_transport_failure_falls_back_to_text(FakeTransportMode::StreamError).await?;
    assert_transport_failure_falls_back_to_text(FakeTransportMode::Oversize).await?;
    Ok(())
}

#[test]
fn notification_fields_are_utf8_safe_and_bounded() {
    let notification =
        NowPlayingNotification::from_media(Generation::new(7), &media(&"Track 🎵".repeat(200)));

    assert!(notification.title().len() <= 256);
    assert!(
        notification
            .creator()
            .is_some_and(|value| value.len() <= 256)
    );
    assert!(
        notification
            .collection()
            .is_some_and(|value| value.len() <= 256)
    );
    assert!(
        notification
            .title()
            .is_char_boundary(notification.title().len())
    );
    assert_eq!(notification.generation(), Generation::new(7));
}

#[test]
fn notification_debug_and_display_redact_provider_identity_and_artwork_url() {
    let mut item = media("secret-title");
    item.creators = vec!["secret-creator".to_owned()];
    item.collection = Some("secret-collection".to_owned());
    let notification = NowPlayingNotification::from_media(Generation::new(7), &item);
    let rendered = format!("{notification:?} {notification}");

    for secret in [
        "secret-provider",
        "secret-video-id",
        "token=secret",
        "example.invalid",
        "secret-title",
        "secret-creator",
        "secret-collection",
    ] {
        assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
    }
    assert!(rendered.contains("NowPlayingNotification"));
    assert!(notification.artwork().is_some());
}

#[test]
fn notification_normalizes_blank_text_to_safe_fallbacks() {
    let mut item = media("   ");
    item.creators = vec![" \n ".to_owned()];
    item.collection = Some("\t".to_owned());
    let notification = NowPlayingNotification::from_media(Generation::new(8), &item);

    assert_eq!(notification.title(), "Unknown title");
    assert_eq!(notification.creator(), None);
    assert_eq!(notification.collection(), None);
}

struct MetadataRecordingBackend {
    sent: tokio::sync::mpsc::UnboundedSender<(String, String)>,
}

#[async_trait]
impl NativeNotificationBackend for MetadataRecordingBackend {
    async fn send(
        &self,
        request: NativeNotificationRequest,
        _cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        self.sent
            .send((request.title().to_owned(), request.body().to_owned()))
            .map_err(|_| RuntimeNotifierError::unavailable())
    }
}

#[tokio::test]
async fn notification_metadata_normalizes_controls_whitespace_and_utf8_before_submission()
-> Result<(), Box<dyn std::error::Error>> {
    let mut item = media(" \r\nTrack\t\u{754c}\u{1b}\u{7}\0\u{81}\u{7f}End\u{2003} ");
    item.creators = vec![" Creator\r\n\tName\u{1b}\u{7} ".to_owned()];
    item.collection = Some(" Album\u{85}\tName\0 ".to_owned());
    let notification = NowPlayingNotification::from_media(Generation::new(9), &item);

    assert_eq!(notification.title(), "Track \u{754c}End");
    assert_eq!(notification.creator(), Some("Creator Name"));
    assert_eq!(notification.collection(), Some("Album Name"));

    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let notifier =
        NativeNotifier::with_text_only_backend(Arc::new(MetadataRecordingBackend { sent }));
    notifier
        .notify(notification.clone(), CancellationToken::new())
        .await?;
    assert_eq!(
        received.recv().await,
        Some((
            "Track \u{754c}End".to_owned(),
            "Creator Name · Album Name".to_owned()
        ))
    );

    let mut controls_only = media("\u{1b}\u{7}\0\u{81}\u{7f}");
    controls_only.creators = vec!["\u{1b}\u{7}\0".to_owned()];
    controls_only.collection = Some("\u{81}\u{7f}".to_owned());
    let controls_only = NowPlayingNotification::from_media(Generation::new(10), &controls_only);
    assert_eq!(controls_only.title(), "Unknown title");
    assert_eq!(controls_only.creator(), None);
    assert_eq!(controls_only.collection(), None);

    let mut boundary = media(&format!("{} \u{e9}", "a".repeat(255)));
    boundary.creators = vec!["\u{754c}".repeat(100)];
    boundary.collection = Some("ordinary é unicode".to_owned());
    let boundary = NowPlayingNotification::from_media(Generation::new(11), &boundary);
    assert_eq!(boundary.title(), "a".repeat(255));
    assert!(boundary.title().len() <= 256);
    assert!(boundary.title().is_char_boundary(boundary.title().len()));
    assert!(boundary.creator().is_some_and(|value| {
        value.len() <= 256 && value.is_char_boundary(value.len()) && value.contains('\u{754c}')
    }));

    let rendered = format!("{notification:?} {notification}");
    assert!(!rendered.contains("Track"));
    assert!(!rendered.contains("Creator"));
    assert!(!rendered.contains("Album"));
    Ok(())
}

#[test]
fn platform_policy_has_no_arbitrary_linux_id_and_requires_registered_windows_identity() {
    assert_eq!(linux_replacement_id(), None);
    assert!(!windows_notifications_supported(None));
    let aum_id = ytermusic::config::WindowsAumId::parse("ExampleCompany.Ytermusic")
        .unwrap_or_else(|error| panic!("valid test AUM ID: {error}"));
    assert!(windows_notifications_supported(Some(&aum_id)));
}

#[test]
fn supported_native_platform_uses_private_path_artwork() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert_eq!(
        NativeNotifier::artwork_mode(),
        NativeArtworkMode::PrivatePngPath
    );
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    assert_eq!(NativeNotifier::artwork_mode(), NativeArtworkMode::None);
    assert_eq!(windows_artwork_mode(), NativeArtworkMode::None);
}

struct StaticPngTransport {
    png: Bytes,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl NotificationArtworkTransport for StaticPngTransport {
    async fn fetch(
        &self,
        _url: &url::Url,
        _cancel: &CancellationToken,
    ) -> Result<NotificationArtworkStream, ArtworkTransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(stream::once({
            let png = self.png.clone();
            async move { Ok(png) }
        })))
    }
}

struct TitleRecordingBackend {
    sent: tokio::sync::mpsc::UnboundedSender<String>,
}

#[async_trait]
impl NativeNotificationBackend for TitleRecordingBackend {
    async fn send(
        &self,
        request: NativeNotificationRequest,
        _cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        self.sent
            .send(request.title().to_owned())
            .map_err(|_| RuntimeNotifierError::unavailable())
    }
}

#[tokio::test]
async fn text_only_prepared_notifier_never_builds_artwork_transport_and_remains_viable()
-> Result<(), Box<dyn std::error::Error>> {
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&factory_calls);
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let notifier = NativeNotifier::from_prepared_services(
        None,
        NativeArtworkMode::PrivatePngPath,
        true,
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(RuntimeNotifierError::unavailable())
        },
        move |_| Arc::new(TitleRecordingBackend { sent }) as Arc<dyn NativeNotificationBackend>,
    )?;

    notifier
        .notify(
            NowPlayingNotification::from_media(Generation::new(43), &media("No artwork client")),
            CancellationToken::new(),
        )
        .await?;

    assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
    assert_eq!(received.recv().await, Some("No artwork client".to_owned()));
    Ok(())
}

#[test]
fn unsupported_prepared_notifier_rejects_before_artwork_factory() {
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&factory_calls);
    let result = NativeNotifier::from_prepared_services(
        None,
        NativeArtworkMode::PrivatePngPath,
        false,
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(RuntimeNotifierError::unavailable())
        },
        |_| {
            Arc::new(CountingBackend(Arc::new(AtomicUsize::new(0))))
                as Arc<dyn NativeNotificationBackend>
        },
    );

    assert!(result.is_err());
    assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_burst_keeps_one_owned_decode_and_cancels_stale_waiters()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let transport_calls = Arc::new(AtomicUsize::new(0));
    let decoder_spawns = Arc::new(AtomicUsize::new(0));
    let active_decoders = Arc::new(AtomicUsize::new(0));
    let maximum_active = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (first_entered_tx, mut first_entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (admission_tx, mut admission_rx) = tokio::sync::mpsc::unbounded_channel();
    let decoder_spawn_count = Arc::clone(&decoder_spawns);
    let decoder_active = Arc::clone(&active_decoders);
    let decoder_maximum = Arc::clone(&maximum_active);
    let decoder_gate = Arc::clone(&gate);
    let loader = BoundedNotificationArtworkLoader::new_with_decoder_and_admission_observer(
        Arc::new(StaticPngTransport {
            png: Bytes::from(png),
            calls: Arc::clone(&transport_calls),
        }),
        Arc::new(move |encoded| {
            let spawn = decoder_spawn_count.fetch_add(1, Ordering::SeqCst);
            let active = decoder_active.fetch_add(1, Ordering::SeqCst) + 1;
            decoder_maximum.fetch_max(active, Ordering::SeqCst);
            if spawn == 0 {
                let _ = first_entered_tx.send(());
                let (released, wake) = &*decoder_gate;
                let mut released = released
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !*released {
                    released = wake
                        .wait(released)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
            let result = PrivatePngAttachment::create(encoded);
            decoder_active.fetch_sub(1, Ordering::SeqCst);
            result
        }),
        Arc::new(move || {
            let _ = admission_tx.send(());
        }),
    );
    let (submitted_tx, mut submitted_rx) = tokio::sync::mpsc::unbounded_channel();
    let notifier = Arc::new(NativeNotifier::with_services(
        Arc::new(loader.clone()),
        Arc::new(TitleRecordingBackend { sent: submitted_tx }),
    ));
    let mut worker = NotificationWorker::new(notifier, Duration::from_secs(30));
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(50),
        &media("First"),
    ));
    tokio::time::timeout(Duration::from_secs(1), admission_rx.recv()).await?;
    tokio::time::timeout(Duration::from_secs(1), first_entered_rx.recv()).await?;

    worker.replace(NowPlayingNotification::from_media(
        Generation::new(51),
        &media("Stale"),
    ));
    tokio::time::timeout(Duration::from_secs(1), admission_rx.recv()).await?;
    for generation in 52..=80 {
        worker.replace(NowPlayingNotification::from_media(
            Generation::new(generation),
            &media(if generation == 80 { "Latest" } else { "Stale" }),
        ));
    }
    tokio::time::timeout(Duration::from_secs(1), admission_rx.recv()).await?;
    assert_eq!(transport_calls.load(Ordering::SeqCst), 1);
    assert_eq!(decoder_spawns.load(Ordering::SeqCst), 1);
    assert_eq!(active_decoders.load(Ordering::SeqCst), 1);
    assert_eq!(maximum_active.load(Ordering::SeqCst), 1);

    let (released, wake) = &*gate;
    *released
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    wake.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), submitted_rx.recv()).await?,
        Some("Latest".to_owned())
    );
    assert_eq!(decoder_spawns.load(Ordering::SeqCst), 2);
    assert_eq!(transport_calls.load(Ordering::SeqCst), 2);
    assert_eq!(maximum_active.load(Ordering::SeqCst), 1);
    worker
        .shutdown(tokio::time::Instant::now() + Duration::from_secs(1))
        .await;
    Ok(())
}

struct InvalidArtworkLoader;

#[async_trait]
impl NotificationArtworkLoader for InvalidArtworkLoader {
    async fn load(
        &self,
        _artwork: Option<&ytermusic::domain::ArtworkUrl>,
        _cancel: &CancellationToken,
    ) -> Option<PrivatePngAttachment> {
        PrivatePngAttachment::create(b"not an image").ok()
    }
}

struct RequestRecordingBackend {
    sent: Mutex<Option<std::sync::mpsc::Sender<NativeNotificationRequest>>>,
    fail: bool,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl NativeNotificationBackend for RequestRecordingBackend {
    async fn send(
        &self,
        request: NativeNotificationRequest,
        _cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(sent) = self
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            sent.send(request)
                .map_err(|_| RuntimeNotifierError::unavailable())?;
        }
        if self.fail {
            Err(RuntimeNotifierError::unavailable())
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn artwork_decode_failure_still_sends_bounded_text_without_native_ui()
-> Result<(), Box<dyn std::error::Error>> {
    let (sent, received) = std::sync::mpsc::channel();
    let notifier = NativeNotifier::with_services(
        Arc::new(InvalidArtworkLoader),
        Arc::new(RequestRecordingBackend {
            sent: Mutex::new(Some(sent)),
            fail: false,
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    );
    notifier
        .notify(
            NowPlayingNotification::from_media(Generation::new(11), &media("Visible title")),
            CancellationToken::new(),
        )
        .await?;
    let request = received.recv()?;
    assert_eq!(request.title(), "Visible title");
    assert!(request.body().contains("Artist"));
    assert!(request.artwork_path().is_none());
    let debug = format!("{request:?}");
    assert!(!debug.contains("Visible title"));
    assert!(!debug.contains("Artist"));
    assert!(!debug.contains("example.invalid"));
    assert!(!debug.contains("token=secret"));
    Ok(())
}

#[test]
fn oversized_artwork_failure_is_static_and_secret_safe() {
    let encoded = vec![b's'; 4 * 1024 * 1024 + 1];
    let Err(error) = PrivatePngAttachment::create(&encoded) else {
        panic!("oversized artwork must fail");
    };
    let rendered = format!("{error:?} {error}");
    assert_eq!(
        rendered,
        "RuntimeNotifierError native notification is unavailable"
    );
    assert!(!rendered.contains("ssss"));
}

struct StaticArtworkLoader {
    png: Vec<u8>,
    path: Mutex<Option<std::sync::mpsc::Sender<std::path::PathBuf>>>,
}

#[async_trait]
impl NotificationArtworkLoader for StaticArtworkLoader {
    async fn load(
        &self,
        _artwork: Option<&ytermusic::domain::ArtworkUrl>,
        _cancel: &CancellationToken,
    ) -> Option<PrivatePngAttachment> {
        let attachment = PrivatePngAttachment::create(&self.png).ok()?;
        if let Some(path) = self
            .path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = path.send(attachment.path().to_owned());
        }
        Some(attachment)
    }
}

#[tokio::test]
async fn backend_error_drops_private_artwork_after_send_returns()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let (path_tx, path_rx) = std::sync::mpsc::channel();
    let notifier = NativeNotifier::with_services(
        Arc::new(StaticArtworkLoader {
            png,
            path: Mutex::new(Some(path_tx)),
        }),
        Arc::new(RequestRecordingBackend {
            sent: Mutex::new(None),
            fail: true,
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    );
    assert!(
        notifier
            .notify(
                NowPlayingNotification::from_media(Generation::new(12), &media("Error")),
                CancellationToken::new(),
            )
            .await
            .is_err()
    );
    let path = path_rx.recv()?;
    assert!(!path.exists());
    Ok(())
}

#[tokio::test]
async fn prepared_artwork_uses_the_exact_private_path_without_secondary_decode()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let (path_tx, path_rx) = std::sync::mpsc::channel();
    let (sent, received) = std::sync::mpsc::channel();
    let notifier = NativeNotifier::with_services(
        Arc::new(StaticArtworkLoader {
            png,
            path: Mutex::new(Some(path_tx)),
        }),
        Arc::new(RequestRecordingBackend {
            sent: Mutex::new(Some(sent)),
            fail: false,
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    );
    notifier
        .notify(
            NowPlayingNotification::from_media(Generation::new(13), &media("Path")),
            CancellationToken::new(),
        )
        .await?;
    let path = path_rx.recv()?;
    let request = received.recv()?;
    assert_eq!(request.artwork_path(), Some(path.as_path()));
    assert!(path.exists());
    drop(request);
    assert!(!path.exists());
    Ok(())
}

struct CancellingArtworkLoader;

#[async_trait]
impl NotificationArtworkLoader for CancellingArtworkLoader {
    async fn load(
        &self,
        _artwork: Option<&ytermusic::domain::ArtworkUrl>,
        cancel: &CancellationToken,
    ) -> Option<PrivatePngAttachment> {
        cancel.cancel();
        None
    }
}

struct CountingBackend(Arc<AtomicUsize>);

#[async_trait]
impl NativeNotificationBackend for CountingBackend {
    async fn send(
        &self,
        _request: NativeNotificationRequest,
        _cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn cancellation_after_artwork_loading_never_invokes_backend() {
    let calls = Arc::new(AtomicUsize::new(0));
    let notifier = NativeNotifier::with_services(
        Arc::new(CancellingArtworkLoader),
        Arc::new(CountingBackend(Arc::clone(&calls))),
    );
    let result = notifier
        .notify(
            NowPlayingNotification::from_media(Generation::new(14), &media("Cancel")),
            CancellationToken::new(),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pre_cancelled_owned_blocking_stage_never_schedules_work() {
    let calls = Arc::new(AtomicUsize::new(0));
    let task_calls = Arc::clone(&calls);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = run_owned_blocking(cancel, move || {
        task_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
    .await;
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[derive(Default)]
struct BlockingNotifier {
    started: Notify,
    completed: Mutex<Vec<u64>>,
}

#[derive(Default)]
struct NonCooperativeNotifier {
    started: Notify,
}

#[async_trait]
impl RuntimeNotifier for NonCooperativeNotifier {
    async fn notify(
        &self,
        _notification: NowPlayingNotification,
        _cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        self.started.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn notification_shutdown_deadline_detaches_non_cooperative_operation() {
    let notifier = Arc::new(NonCooperativeNotifier::default());
    let mut worker = NotificationWorker::new(notifier.clone(), Duration::from_secs(30));
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(20),
        &media("Never returns"),
    ));
    notifier.started.notified().await;

    let shutdown = tokio::time::timeout(
        Duration::from_millis(50),
        worker.shutdown(tokio::time::Instant::now() + Duration::from_millis(25)),
    )
    .await;
    assert!(
        shutdown.is_ok(),
        "notification shutdown exceeded its deadline"
    );
}

struct CoalescingNotifier {
    started: tokio::sync::mpsc::UnboundedSender<u64>,
    release_first: Notify,
}

#[async_trait]
impl RuntimeNotifier for CoalescingNotifier {
    async fn notify(
        &self,
        notification: NowPlayingNotification,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        let generation = notification.generation().value();
        self.started
            .send(generation)
            .map_err(|_| RuntimeNotifierError::unavailable())?;
        cancel.cancelled().await;
        if generation == 1 {
            self.release_first.notified().await;
        }
        Ok(())
    }
}

#[tokio::test]
async fn replacements_coalesce_to_latest_while_cancelled_stage_joins()
-> Result<(), Box<dyn std::error::Error>> {
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let notifier = Arc::new(CoalescingNotifier {
        started,
        release_first: Notify::new(),
    });
    let mut worker = NotificationWorker::new(notifier.clone(), Duration::from_secs(30));
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(1),
        &media("One"),
    ));
    assert_eq!(starts.recv().await, Some(1));
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(2),
        &media("Two"),
    ));
    tokio::task::yield_now().await;
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(3),
        &media("Three"),
    ));
    notifier.release_first.notify_one();
    assert_eq!(starts.recv().await, Some(3));
    worker
        .shutdown(tokio::time::Instant::now() + Duration::from_secs(1))
        .await;
    Ok(())
}

#[async_trait]
impl RuntimeNotifier for BlockingNotifier {
    async fn notify(
        &self,
        notification: NowPlayingNotification,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        self.started.notify_one();
        cancel.cancelled().await;
        self.completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(notification.generation().value());
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn notification_worker_replaces_and_times_out_bounded_tasks() {
    let notifier = Arc::new(BlockingNotifier::default());
    let mut worker = NotificationWorker::new(notifier.clone(), Duration::from_millis(100));

    let (): () = worker.replace(NowPlayingNotification::from_media(
        Generation::new(1),
        &media("One"),
    ));
    notifier.started.notified().await;
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(2),
        &media("Two"),
    ));
    notifier.started.notified().await;
    tokio::time::advance(Duration::from_millis(101)).await;
    tokio::task::yield_now().await;

    assert_eq!(worker.diagnostic_count(), 1);
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(3),
        &media("Three"),
    ));
    notifier.started.notified().await;
    tokio::time::advance(Duration::from_millis(101)).await;
    tokio::task::yield_now().await;
    assert_eq!(worker.diagnostic_count(), 1);
    assert_eq!(
        *notifier
            .completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![1, 2, 3]
    );
    worker
        .shutdown(tokio::time::Instant::now() + Duration::from_secs(1))
        .await;
}

#[derive(Default)]
struct ErrorNotifier;

#[async_trait]
impl RuntimeNotifier for ErrorNotifier {
    async fn notify(
        &self,
        _notification: NowPlayingNotification,
        _cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        Err(RuntimeNotifierError::unavailable())
    }
}

#[tokio::test]
async fn notifier_errors_are_redacted_and_nonfatal() {
    let Err(error) = ErrorNotifier
        .notify(
            NowPlayingNotification::from_media(Generation::new(3), &media("Three")),
            CancellationToken::new(),
        )
        .await
    else {
        panic!("test backend must fail");
    };
    assert_eq!(error.to_string(), "native notification is unavailable");

    let mut worker = NotificationWorker::new(Arc::new(ErrorNotifier), Duration::from_secs(1));
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(3),
        &media("Three"),
    ));
    tokio::task::yield_now().await;
    assert_eq!(worker.diagnostic_count(), 1);
    worker
        .shutdown(tokio::time::Instant::now() + Duration::from_secs(1))
        .await;
}

#[test]
fn private_png_attachment_is_mode_0600_and_removed_on_drop()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let attachment = PrivatePngAttachment::create(&png)?;
    let path = attachment.path().to_owned();
    assert!(path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path)?.permissions().mode() & 0o777,
            0o600
        );
    }
    drop(attachment);
    assert!(!path.exists());
    Ok(())
}

struct PathRecordingSubmitter {
    sent: tokio::sync::mpsc::UnboundedSender<std::path::PathBuf>,
}

impl NativeNotificationSubmitter for PathRecordingSubmitter {
    fn submit(&self, request: &NativeSubmissionRequest) -> Result<(), RuntimeNotifierError> {
        self.sent
            .send(
                request
                    .artwork_path()
                    .ok_or_else(RuntimeNotifierError::unavailable)?
                    .to_owned(),
            )
            .map_err(|_| RuntimeNotifierError::unavailable())
    }
}

#[tokio::test]
async fn notification_cache_prunes_startup_and_retains_only_two_private_bounded_files()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let directory = root.path().join("notifications");
    std::fs::create_dir_all(&directory)?;
    let stale = directory.join("now-playing-3-0.png");
    std::fs::write(&stale, b"stale")?;

    let cache = Arc::new(NotificationArtworkCache::new(root.path())?);
    assert!(!stale.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&directory)?.permissions().mode() & 0o777,
            0o700
        );
    }

    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let (path_tx, mut path_rx) = tokio::sync::mpsc::unbounded_channel();
    let notifier = NativeNotifier::with_services(
        Arc::new(StaticArtworkLoader {
            png,
            path: Mutex::new(None),
        }),
        Arc::new(CommittedNativeNotificationBackend::new(
            Arc::clone(&cache),
            Arc::new(PathRecordingSubmitter { sent: path_tx }),
        )),
    );
    let mut first_path = None;
    for generation in 1..=3 {
        notifier
            .notify(
                NowPlayingNotification::from_media(Generation::new(generation), &media("Cached")),
                CancellationToken::new(),
            )
            .await?;
        let cached_path = tokio::time::timeout(Duration::from_secs(1), path_rx.recv())
            .await?
            .ok_or("cached path missing")?;
        assert!(cached_path.exists());
        assert!(std::fs::read_dir(&directory)?.count() <= 2);
        assert!(std::fs::metadata(&cached_path)?.len() <= 4 * 1024 * 1024);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&cached_path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        first_path.get_or_insert(cached_path);
    }

    let retained = cache.retained_paths();
    assert_eq!(retained.len(), 2);
    assert!(!first_path.ok_or("first cache path missing")?.exists());
    assert!(retained.iter().all(|path| path.exists()));
    drop(notifier);
    drop(cache);
    assert!(
        retained.iter().all(|path| path.exists()),
        "shutdown must leave accepted artwork for deferred platform loading"
    );
    let restarted = NotificationArtworkCache::new(root.path())?;
    assert!(retained.iter().all(|path| !path.exists()));
    assert!(restarted.retained_paths().is_empty());
    Ok(())
}

struct BlockingSubmitter {
    calls: Arc<AtomicUsize>,
    started: Mutex<Option<std::sync::mpsc::Sender<std::path::PathBuf>>>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

struct TitleRecordingSubmitter {
    sent: tokio::sync::mpsc::UnboundedSender<String>,
}

impl NativeNotificationSubmitter for TitleRecordingSubmitter {
    fn submit(&self, request: &NativeSubmissionRequest) -> Result<(), RuntimeNotifierError> {
        self.sent
            .send(request.title().to_owned())
            .map_err(|_| RuntimeNotifierError::unavailable())
    }
}

#[tokio::test]
async fn text_only_notifier_submits_without_an_artwork_cache()
-> Result<(), Box<dyn std::error::Error>> {
    let (submitted_tx, mut submitted_rx) = tokio::sync::mpsc::unbounded_channel();
    let notifier = NativeNotifier::with_text_only_backend(Arc::new(
        CommittedNativeNotificationBackend::new_text_only(Arc::new(TitleRecordingSubmitter {
            sent: submitted_tx,
        })),
    ));

    notifier
        .notify(
            NowPlayingNotification::from_media(Generation::new(42), &media("Text only")),
            CancellationToken::new(),
        )
        .await?;

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), submitted_rx.recv()).await?,
        Some("Text only".to_owned())
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_promotion_keeps_commit_permit_until_submission_finishes()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let promotion_calls = Arc::new(AtomicUsize::new(0));
    let (first_entered_tx, mut first_entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let (second_entered_tx, mut second_entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let observer_calls = Arc::clone(&promotion_calls);
    let observer_gate = Arc::clone(&gate);
    let cache = Arc::new(NotificationArtworkCache::new_with_promotion_observer(
        root.path(),
        Arc::new(move || {
            let call = observer_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                let _ = first_entered_tx.send(());
                let (released, wake) = &*observer_gate;
                let mut released = released
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                while !*released {
                    released = wake
                        .wait(released)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            } else {
                let _ = second_entered_tx.send(());
            }
        }),
    )?);
    let (submitted_tx, mut submitted_rx) = tokio::sync::mpsc::unbounded_channel();
    let backend = Arc::new(CommittedNativeNotificationBackend::new(
        Arc::clone(&cache),
        Arc::new(TitleRecordingSubmitter { sent: submitted_tx }),
    ));
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let notifier = Arc::new(NativeNotifier::with_services(
        Arc::new(StaticArtworkLoader {
            png,
            path: Mutex::new(None),
        }),
        backend,
    ));
    let first_cancel = CancellationToken::new();
    let first_notifier = Arc::clone(&notifier);
    let first_task_cancel = first_cancel.clone();
    let mut first_task = tokio::spawn(async move {
        first_notifier
            .notify(
                NowPlayingNotification::from_media(Generation::new(40), &media("First")),
                first_task_cancel,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), first_entered_rx.recv()).await?;
    assert!(std::fs::read_dir(root.path().join("notifications"))?.count() <= 2);

    first_cancel.cancel();
    let first_completion = tokio::time::timeout(Duration::from_millis(250), &mut first_task)
        .await
        .ok();
    let latest_notifier = Arc::clone(&notifier);
    let latest_task = tokio::spawn(async move {
        latest_notifier
            .notify(
                NowPlayingNotification::from_media(Generation::new(41), &media("Latest")),
                CancellationToken::new(),
            )
            .await
    });
    let latest_entered_before_release =
        tokio::time::timeout(Duration::from_millis(50), second_entered_rx.recv())
            .await
            .is_ok();
    assert!(std::fs::read_dir(root.path().join("notifications"))?.count() <= 2);

    let (released, wake) = &*gate;
    *released
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    wake.notify_one();
    assert!(
        !latest_entered_before_release,
        "replacement entered promotion before the detached commit released its permit"
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), submitted_rx.recv()).await?,
        Some("First".to_owned())
    );
    tokio::time::timeout(Duration::from_secs(1), second_entered_rx.recv()).await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), submitted_rx.recv()).await?,
        Some("Latest".to_owned())
    );
    assert_eq!(promotion_calls.load(Ordering::SeqCst), 2);
    assert!(std::fs::read_dir(root.path().join("notifications"))?.count() <= 2);
    if let Some(result) = first_completion {
        result??;
    } else {
        first_task.await??;
    }
    latest_task.await??;
    Ok(())
}

impl NativeNotificationSubmitter for BlockingSubmitter {
    fn submit(&self, request: &NativeSubmissionRequest) -> Result<(), RuntimeNotifierError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let path = request
            .artwork_path()
            .ok_or_else(RuntimeNotifierError::unavailable)?
            .to_owned();
        if let Some(started) = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            started
                .send(path)
                .map_err(|_| RuntimeNotifierError::unavailable())?;
        }
        let (released, wake) = &*self.gate;
        let mut released = released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*released {
            released = wake
                .wait(released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_submission_cancels_before_commit_but_owns_after_commit()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let root = tempfile::tempdir()?;
    let cache = Arc::new(NotificationArtworkCache::new(root.path())?);
    let calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let backend = Arc::new(CommittedNativeNotificationBackend::new(
        Arc::clone(&cache),
        Arc::new(BlockingSubmitter {
            calls: Arc::clone(&calls),
            started: Mutex::new(Some(started_tx)),
            gate: Arc::clone(&gate),
        }),
    ));
    let notifier = Arc::new(NativeNotifier::with_services(
        Arc::new(StaticArtworkLoader {
            png: png.clone(),
            path: Mutex::new(None),
        }),
        backend,
    ));

    let pre_cancel = CancellationToken::new();
    pre_cancel.cancel();
    assert!(
        notifier
            .notify(
                NowPlayingNotification::from_media(Generation::new(30), &media("Pre")),
                pre_cancel,
            )
            .await
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task_notifier = Arc::clone(&notifier);
    let task = tokio::spawn(async move {
        task_notifier
            .notify(
                NowPlayingNotification::from_media(Generation::new(31), &media("Committed")),
                task_cancel,
            )
            .await
    });
    let committed_path = tokio::task::spawn_blocking(move || started_rx.recv()).await??;
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!task.is_finished());
    assert!(committed_path.exists());
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let (released, wake) = &*gate;
    *released
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    wake.notify_one();
    task.await??;
    assert_eq!(cache.retained_paths(), vec![committed_path]);
    Ok(())
}

#[test]
fn private_png_attachment_rejects_excessive_decode_dimensions()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2_049, 1, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;

    assert!(matches!(
        PrivatePngAttachment::create(&png),
        Err(RuntimeNotifierError)
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_owned_blocking_stage_detaches_and_retains_attachment_until_thread_returns()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (path_tx, path_rx) = std::sync::mpsc::channel();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task_gate = Arc::clone(&gate);
    let mut task = tokio::spawn(async move {
        run_owned_blocking(task_cancel, move || {
            let attachment = PrivatePngAttachment::create(&png)?;
            path_tx
                .send(attachment.path().to_owned())
                .map_err(|_| RuntimeNotifierError::unavailable())?;
            let (released, wake) = &*task_gate;
            let mut released = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            Ok(attachment)
        })
        .await
    });
    let path = tokio::task::spawn_blocking(move || path_rx.recv()).await??;
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_millis(100), &mut task).await;
    assert!(path.exists());
    {
        let (released, wake) = &*gate;
        *released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        wake.notify_one();
    }
    if outcome.is_err() {
        let _ = task.await;
    }
    assert!(
        matches!(outcome, Ok(Ok(Err(RuntimeNotifierError)))),
        "cancellation waited for blocking work instead of detaching"
    );
    for _ in 0..100 {
        if !path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(
        !path.exists(),
        "detached owner did not clean after returning"
    );
    Ok(())
}

struct AttachmentBlockingNotifier {
    png: Vec<u8>,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    path: Mutex<Option<std::sync::mpsc::Sender<std::path::PathBuf>>>,
    completed: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

#[async_trait]
impl RuntimeNotifier for AttachmentBlockingNotifier {
    async fn notify(
        &self,
        _notification: NowPlayingNotification,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        let png = self.png.clone();
        let gate = Arc::clone(&self.gate);
        let completed = self
            .completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let path = self
            .path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(RuntimeNotifierError::unavailable)?;
        run_owned_blocking(cancel, move || {
            let attachment = PrivatePngAttachment::create(&png)?;
            path.send(attachment.path().to_owned())
                .map_err(|_| RuntimeNotifierError::unavailable())?;
            let (released, wake) = &*gate;
            let mut released = released
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            drop(attachment);
            if let Some(completed) = completed {
                let _ = completed.send(());
            }
            Ok(())
        })
        .await
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_reports_cancellation_while_detached_attachment_cleanup_continues()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (path_tx, path_rx) = std::sync::mpsc::channel();
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let notifier = Arc::new(AttachmentBlockingNotifier {
        png,
        gate: Arc::clone(&gate),
        path: Mutex::new(Some(path_tx)),
        completed: Mutex::new(Some(completed_tx)),
    });
    let mut worker = NotificationWorker::new(notifier, Duration::from_millis(100));
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(9),
        &media("Timeout"),
    ));
    let path = tokio::task::spawn_blocking(move || path_rx.recv()).await??;
    tokio::time::advance(Duration::from_millis(101)).await;
    tokio::task::yield_now().await;
    assert!(path.exists());
    assert_eq!(worker.diagnostic_count(), 1);
    {
        let (released, wake) = &*gate;
        *released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        wake.notify_one();
    }
    tokio::task::spawn_blocking(move || completed_rx.recv()).await??;
    assert!(!path.exists());
    worker
        .shutdown(tokio::time::Instant::now() + Duration::from_secs(1))
        .await;
    Ok(())
}

#[tokio::test]
async fn shutdown_detaches_owned_attachment_cleanup_at_deadline()
-> Result<(), Box<dyn std::error::Error>> {
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 2, Rgba([1, 2, 3, 255])))
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let (path_tx, path_rx) = std::sync::mpsc::channel();
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let notifier = Arc::new(AttachmentBlockingNotifier {
        png,
        gate: Arc::clone(&gate),
        path: Mutex::new(Some(path_tx)),
        completed: Mutex::new(Some(completed_tx)),
    });
    let mut worker = NotificationWorker::new(notifier, Duration::from_secs(30));
    worker.replace(NowPlayingNotification::from_media(
        Generation::new(10),
        &media("Shutdown"),
    ));
    let path = tokio::task::spawn_blocking(move || path_rx.recv()).await??;
    let shutdown = tokio::spawn(async move {
        worker
            .shutdown(tokio::time::Instant::now() + Duration::from_secs(1))
            .await;
    });
    tokio::task::yield_now().await;
    assert!(shutdown.is_finished());
    assert!(path.exists());
    {
        let (released, wake) = &*gate;
        *released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        wake.notify_one();
    }
    shutdown.await?;
    tokio::task::spawn_blocking(move || completed_rx.recv()).await??;
    assert!(!path.exists());
    Ok(())
}
