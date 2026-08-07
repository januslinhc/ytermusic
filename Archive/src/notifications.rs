use std::{
    collections::VecDeque,
    fmt, fs,
    io::{Cursor, Read as _, Write as _},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt as _};
use thiserror::Error;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    app::Generation,
    domain::{ArtworkUrl, MediaItem},
};

const MAX_TEXT_BYTES: usize = 256;
const MAX_NOTIFICATION_ARTWORK_BYTES: usize = 4 * 1024 * 1024;
const MAX_NOTIFICATION_ARTWORK_EDGE: u32 = 512;
const MAX_NOTIFICATION_DECODE_EDGE: u32 = 2_048;
const MAX_NOTIFICATION_DECODE_PIXELS: u64 = 4_000_000;
const MAX_RETAINED_NOTIFICATION_ARTWORK: usize = 2;

#[derive(Clone, PartialEq)]
pub struct NowPlayingNotification {
    generation: Generation,
    title: String,
    creator: Option<String>,
    collection: Option<String>,
    artwork: Option<ArtworkUrl>,
}

impl NowPlayingNotification {
    #[must_use]
    pub fn from_media(generation: Generation, media: &MediaItem) -> Self {
        Self {
            generation,
            title: normalized_text(&media.title).unwrap_or_else(|| "Unknown title".to_owned()),
            creator: media
                .creators
                .iter()
                .find_map(|value| normalized_text(value)),
            collection: media.collection.as_deref().and_then(normalized_text),
            artwork: media
                .artwork_url
                .clone()
                .and_then(|url| ArtworkUrl::try_from(url).ok()),
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn creator(&self) -> Option<&str> {
        self.creator.as_deref()
    }

    #[must_use]
    pub fn collection(&self) -> Option<&str> {
        self.collection.as_deref()
    }

    #[must_use]
    pub const fn artwork(&self) -> Option<&ArtworkUrl> {
        self.artwork.as_ref()
    }
}

impl fmt::Debug for NowPlayingNotification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NowPlayingNotification")
            .field("generation", &self.generation)
            .field("title_present", &!self.title.is_empty())
            .field("creator_present", &self.creator.is_some())
            .field("collection_present", &self.collection.is_some())
            .field("artwork_present", &self.artwork.is_some())
            .finish()
    }
}

impl fmt::Display for NowPlayingNotification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NowPlayingNotification([REDACTED metadata])")
    }
}

fn bounded_text(value: &str) -> String {
    let mut end = value.len().min(MAX_TEXT_BYTES);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_owned()
}

fn normalized_text(value: &str) -> Option<String> {
    let mut normalized = String::with_capacity(value.len().min(MAX_TEXT_BYTES));
    let mut whitespace_pending = false;
    for character in value.chars() {
        if character.is_whitespace() {
            whitespace_pending = !normalized.is_empty();
        } else if !character.is_control() {
            let separator_bytes = usize::from(whitespace_pending);
            if normalized
                .len()
                .saturating_add(separator_bytes)
                .saturating_add(character.len_utf8())
                > MAX_TEXT_BYTES
            {
                break;
            }
            if whitespace_pending {
                normalized.push(' ');
            }
            normalized.push(character);
            whitespace_pending = false;
        }
    }
    let bounded = bounded_text(&normalized);
    let bounded = bounded.trim_end();
    (!bounded.is_empty()).then(|| bounded.to_owned())
}

pub struct PrivatePngAttachment {
    file: tempfile::NamedTempFile,
}

impl PrivatePngAttachment {
    /// Creates a bounded, decoded PNG in a private temporary file.
    ///
    /// # Errors
    ///
    /// Returns a redacted availability error for invalid, oversized, or
    /// unwritable image data.
    pub fn create(encoded: &[u8]) -> Result<Self, RuntimeNotifierError> {
        if encoded.len() > MAX_NOTIFICATION_ARTWORK_BYTES {
            return Err(RuntimeNotifierError);
        }
        let (width, height) = image::ImageReader::new(Cursor::new(encoded))
            .with_guessed_format()
            .map_err(|_| RuntimeNotifierError)?
            .into_dimensions()
            .map_err(|_| RuntimeNotifierError)?;
        if width > MAX_NOTIFICATION_DECODE_EDGE
            || height > MAX_NOTIFICATION_DECODE_EDGE
            || u64::from(width).saturating_mul(u64::from(height)) > MAX_NOTIFICATION_DECODE_PIXELS
        {
            return Err(RuntimeNotifierError);
        }
        let decoded = image::load_from_memory(encoded).map_err(|_| RuntimeNotifierError)?;
        let bounded =
            decoded.thumbnail(MAX_NOTIFICATION_ARTWORK_EDGE, MAX_NOTIFICATION_ARTWORK_EDGE);
        let mut png = Vec::new();
        bounded
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(|_| RuntimeNotifierError)?;
        if png.len() > MAX_NOTIFICATION_ARTWORK_BYTES {
            return Err(RuntimeNotifierError);
        }
        let mut file = tempfile::Builder::new()
            .prefix("ytermusic-artwork-")
            .suffix(".png")
            .tempfile()
            .map_err(|_| RuntimeNotifierError)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| RuntimeNotifierError)?;
        }
        file.write_all(&png).map_err(|_| RuntimeNotifierError)?;
        file.flush().map_err(|_| RuntimeNotifierError)?;
        Ok(Self { file })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

impl fmt::Debug for PrivatePngAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PrivatePngAttachment([REDACTED path])")
    }
}

pub struct CachedNotificationArtwork {
    path: PathBuf,
    delete_on_drop: bool,
}

impl CachedNotificationArtwork {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CachedNotificationArtwork {
    fn drop(&mut self) {
        if self.delete_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl fmt::Debug for CachedNotificationArtwork {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CachedNotificationArtwork([REDACTED path])")
    }
}

pub struct NotificationArtworkCache {
    directory: PathBuf,
    next_file: AtomicU64,
    retained: std::sync::Mutex<VecDeque<CachedNotificationArtwork>>,
    promotion_observer: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn validate_notification_cache_directory(
    metadata: &fs::Metadata,
) -> Result<(), RuntimeNotifierError> {
    let file_type = metadata.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        return Err(RuntimeNotifierError);
    }
    Ok(())
}

fn validate_notification_cache_path(path: &Path) -> Result<(), RuntimeNotifierError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RuntimeNotifierError)?;
    validate_notification_cache_directory(&metadata)
}

fn is_owned_notification_artwork_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let Some(numbers) = name
        .strip_prefix("now-playing-")
        .and_then(|name| name.strip_suffix(".png"))
    else {
        return false;
    };
    let Some((generation, sequence)) = numbers.split_once('-') else {
        return false;
    };
    if sequence.contains('-') {
        return false;
    }
    let Ok(parsed_generation) = generation.parse::<u64>() else {
        return false;
    };
    let Ok(parsed_sequence) = sequence.parse::<u64>() else {
        return false;
    };
    parsed_generation.to_string() == generation && parsed_sequence.to_string() == sequence
}

impl NotificationArtworkCache {
    /// Opens the private notification cache and removes earlier-run leftovers.
    ///
    /// # Errors
    ///
    /// Returns a static error when the directory cannot be safely created or inspected.
    pub fn new(cache_root: &Path) -> Result<Self, RuntimeNotifierError> {
        Self::new_inner(cache_root, None)
    }

    #[doc(hidden)]
    pub fn new_with_promotion_observer(
        cache_root: &Path,
        promotion_observer: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, RuntimeNotifierError> {
        Self::new_inner(cache_root, Some(promotion_observer))
    }

    fn new_inner(
        cache_root: &Path,
        promotion_observer: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Self, RuntimeNotifierError> {
        let directory = cache_root.join("notifications");
        fs::create_dir_all(cache_root).map_err(|_| RuntimeNotifierError)?;
        match fs::symlink_metadata(&directory) {
            Ok(metadata) => validate_notification_cache_directory(&metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(RuntimeNotifierError),
                }
                validate_notification_cache_path(&directory)?;
            }
            Err(_) => return Err(RuntimeNotifierError),
        }
        validate_notification_cache_path(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .map_err(|_| RuntimeNotifierError)?;
        }
        validate_notification_cache_path(&directory)?;
        for entry in fs::read_dir(&directory).map_err(|_| RuntimeNotifierError)? {
            let entry = entry.map_err(|_| RuntimeNotifierError)?;
            let file_type = entry.file_type().map_err(|_| RuntimeNotifierError)?;
            if file_type.is_file() && is_owned_notification_artwork_name(&entry.file_name()) {
                if validate_notification_cache_path(&directory).is_err() {
                    return Err(RuntimeNotifierError);
                }
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(Self {
            directory,
            next_file: AtomicU64::new(0),
            retained: std::sync::Mutex::new(VecDeque::new()),
            promotion_observer,
        })
    }

    /// Copies validated artwork into the app-private cache after commit
    /// ownership has transferred to the detached operation.
    ///
    /// This method does not serialize itself. Its only caller must hold the
    /// backend commit permit for the entire promotion and native submission.
    ///
    /// # Errors
    ///
    /// Returns a static error when the bounded private copy cannot be created.
    fn promote(
        &self,
        generation: Generation,
        temporary: PrivatePngAttachment,
    ) -> Result<CachedNotificationArtwork, RuntimeNotifierError> {
        {
            let mut retained = self
                .retained
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while retained.len() >= MAX_RETAINED_NOTIFICATION_ARTWORK {
                retained.pop_front();
            }
        }
        let sequence = self.next_file.fetch_add(1, Ordering::Relaxed);
        let path = self
            .directory
            .join(format!("now-playing-{}-{sequence}.png", generation.value()));
        let result = (|| {
            let source = fs::File::open(temporary.path()).map_err(|_| RuntimeNotifierError)?;
            let mut destination = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(|_| RuntimeNotifierError)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                destination
                    .set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(|_| RuntimeNotifierError)?;
            }
            if let Some(observer) = &self.promotion_observer {
                observer();
            }
            let copied = std::io::copy(
                &mut source.take(MAX_NOTIFICATION_ARTWORK_BYTES as u64 + 1),
                &mut destination,
            )
            .map_err(|_| RuntimeNotifierError)?;
            if copied > MAX_NOTIFICATION_ARTWORK_BYTES as u64 {
                return Err(RuntimeNotifierError);
            }
            destination.flush().map_err(|_| RuntimeNotifierError)?;
            Ok(CachedNotificationArtwork {
                path: path.clone(),
                delete_on_drop: true,
            })
        })();
        drop(temporary);
        if result.is_err() {
            let _ = fs::remove_file(&path);
        }
        result
    }

    pub fn retain(&self, artwork: CachedNotificationArtwork) {
        let mut retained = self
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        retained.push_back(artwork);
        while retained.len() > MAX_RETAINED_NOTIFICATION_ARTWORK {
            retained.pop_front();
        }
    }

    #[must_use]
    pub fn retained_paths(&self) -> Vec<PathBuf> {
        self.retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|artwork| artwork.path.clone())
            .collect()
    }
}

impl fmt::Debug for NotificationArtworkCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NotificationArtworkCache")
            .field("retained", &self.retained_paths().len())
            .finish()
    }
}

impl Drop for NotificationArtworkCache {
    fn drop(&mut self) {
        let retained = self
            .retained
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for artwork in retained {
            artwork.delete_on_drop = false;
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("native notification is unavailable")]
pub struct RuntimeNotifierError;

impl RuntimeNotifierError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self
    }
}

/// Runs synchronous work on an independently owned operating-system thread.
///
/// Cancellation stops awaiting the result. The detached thread retains every
/// captured resource until the synchronous operation actually returns, and it
/// does not participate in Tokio runtime shutdown.
///
/// # Errors
///
/// Returns a static error when the operation fails, panics, or is cancelled.
pub async fn run_owned_blocking<T, F>(
    cancel: CancellationToken,
    operation: F,
) -> Result<T, RuntimeNotifierError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, RuntimeNotifierError> + Send + 'static,
{
    if cancel.is_cancelled() {
        return Err(RuntimeNotifierError);
    }
    let (result_tx, mut result_rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("ytermusic-notification".to_owned())
        .spawn(move || {
            let _ = result_tx.send(operation());
        })
        .map_err(|_| RuntimeNotifierError)?;
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(RuntimeNotifierError),
        result = &mut result_rx => result.map_err(|_| RuntimeNotifierError)?,
    }
}

/// Prepares the notification artwork cache on an owned OS thread before the
/// enabled/platform gate, then constructs an optional nonblocking service.
///
/// A cache error or timeout is represented as `None` for the constructor so a
/// text-only service can remain available. Disabled notifications still run
/// the bounded preparation for startup pruning but skip construction.
#[doc(hidden)]
pub async fn initialize_notification_service<T, Prepare, Construct>(
    enabled: bool,
    timeout: Duration,
    prepare: Prepare,
    construct: Construct,
) -> Option<T>
where
    Prepare: FnOnce() -> Result<NotificationArtworkCache, RuntimeNotifierError> + Send + 'static,
    Construct: FnOnce(Option<Arc<NotificationArtworkCache>>) -> Result<T, RuntimeNotifierError>,
{
    let cancel = CancellationToken::new();
    let mut operation = Box::pin(run_owned_blocking(cancel.clone(), prepare));
    let cache = tokio::select! {
        biased;
        result = &mut operation => result.ok().map(Arc::new),
        () = tokio::time::sleep(timeout) => {
            cancel.cancel();
            None
        }
    };
    if !enabled {
        return None;
    }
    construct(cache).ok()
}

#[async_trait]
pub trait RuntimeNotifier: Send + Sync {
    async fn notify(
        &self,
        notification: NowPlayingNotification,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError>;
}

pub struct NativeNotificationRequest {
    generation: Generation,
    title: String,
    body: String,
    artwork: Option<PrivatePngAttachment>,
}

impl NativeNotificationRequest {
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    #[must_use]
    pub fn artwork_path(&self) -> Option<&Path> {
        self.artwork.as_ref().map(PrivatePngAttachment::path)
    }
}

impl fmt::Debug for NativeNotificationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeNotificationRequest")
            .field("generation", &self.generation)
            .field("title_present", &!self.title.is_empty())
            .field("body_present", &!self.body.is_empty())
            .field("artwork_present", &self.artwork.is_some())
            .finish()
    }
}

pub struct NativeSubmissionRequest {
    title: String,
    body: String,
    artwork: Option<CachedNotificationArtwork>,
}

impl NativeSubmissionRequest {
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    #[must_use]
    pub fn artwork_path(&self) -> Option<&Path> {
        self.artwork.as_ref().map(CachedNotificationArtwork::path)
    }
}

impl fmt::Debug for NativeSubmissionRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeSubmissionRequest")
            .field("title_present", &!self.title.is_empty())
            .field("body_present", &!self.body.is_empty())
            .field("artwork_present", &self.artwork.is_some())
            .finish()
    }
}

pub trait NativeNotificationSubmitter: Send + Sync {
    /// Submits a request to the platform and returns after native acceptance.
    ///
    /// # Errors
    ///
    /// Returns a static error when the platform rejects the request.
    fn submit(&self, request: &NativeSubmissionRequest) -> Result<(), RuntimeNotifierError>;
}

#[async_trait]
pub trait NotificationArtworkLoader: Send + Sync {
    async fn load(
        &self,
        artwork: Option<&ArtworkUrl>,
        cancel: &CancellationToken,
    ) -> Option<PrivatePngAttachment>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("notification artwork transport is unavailable")]
pub struct ArtworkTransportError;

impl ArtworkTransportError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self
    }
}

pub type NotificationArtworkStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, ArtworkTransportError>> + Send>>;

#[async_trait]
pub trait NotificationArtworkTransport: Send + Sync {
    async fn fetch(
        &self,
        url: &url::Url,
        cancel: &CancellationToken,
    ) -> Result<NotificationArtworkStream, ArtworkTransportError>;
}

#[async_trait]
pub trait NativeNotificationBackend: Send + Sync {
    async fn send(
        &self,
        request: NativeNotificationRequest,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError>;
}

pub struct CommittedNativeNotificationBackend {
    cache: Option<Arc<NotificationArtworkCache>>,
    submitter: Arc<dyn NativeNotificationSubmitter>,
    commit: Arc<tokio::sync::Semaphore>,
}

impl CommittedNativeNotificationBackend {
    #[must_use]
    pub fn new(
        cache: Arc<NotificationArtworkCache>,
        submitter: Arc<dyn NativeNotificationSubmitter>,
    ) -> Self {
        Self {
            cache: Some(cache),
            submitter,
            commit: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    #[must_use]
    pub fn new_text_only(submitter: Arc<dyn NativeNotificationSubmitter>) -> Self {
        Self {
            cache: None,
            submitter,
            commit: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

#[async_trait]
impl NativeNotificationBackend for CommittedNativeNotificationBackend {
    async fn send(
        &self,
        request: NativeNotificationRequest,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        if cancel.is_cancelled() {
            return Err(RuntimeNotifierError);
        }
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(RuntimeNotifierError),
            permit = Arc::clone(&self.commit).acquire_owned() => permit.map_err(|_| RuntimeNotifierError)?,
        };
        let NativeNotificationRequest {
            generation,
            title,
            body,
            artwork,
        } = request;
        if cancel.is_cancelled() {
            return Err(RuntimeNotifierError);
        }
        let submitter = Arc::clone(&self.submitter);
        let cache = self.cache.clone();
        run_owned_blocking(CancellationToken::new(), move || {
            let _permit = permit;
            let artwork = artwork.and_then(|temporary| {
                cache
                    .as_ref()
                    .and_then(|cache| cache.promote(generation, temporary).ok())
            });
            let mut request = NativeSubmissionRequest {
                title,
                body,
                artwork,
            };
            let result = submitter.submit(&request);
            if result.is_ok()
                && let Some(artwork) = request.artwork.take()
                && let Some(cache) = cache.as_ref()
            {
                cache.retain(artwork);
            }
            result
        })
        .await
    }
}

struct ReqwestNotificationArtworkTransport {
    client: reqwest::Client,
}

type NotificationArtworkDecoder =
    dyn Fn(&[u8]) -> Result<PrivatePngAttachment, RuntimeNotifierError> + Send + Sync;

#[derive(Clone)]
pub struct BoundedNotificationArtworkLoader {
    transport: Arc<dyn NotificationArtworkTransport>,
    decoder: Arc<NotificationArtworkDecoder>,
    decode_admission: Arc<tokio::sync::Semaphore>,
    admission_observer: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl BoundedNotificationArtworkLoader {
    #[must_use]
    pub fn new(transport: Arc<dyn NotificationArtworkTransport>) -> Self {
        Self::new_with_decoder(transport, Arc::new(PrivatePngAttachment::create))
    }

    fn new_with_decoder(
        transport: Arc<dyn NotificationArtworkTransport>,
        decoder: Arc<NotificationArtworkDecoder>,
    ) -> Self {
        Self {
            transport,
            decoder,
            decode_admission: Arc::new(tokio::sync::Semaphore::new(1)),
            admission_observer: None,
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub fn new_with_decoder_and_admission_observer(
        transport: Arc<dyn NotificationArtworkTransport>,
        decoder: Arc<NotificationArtworkDecoder>,
        admission_observer: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            transport,
            decoder,
            decode_admission: Arc::new(tokio::sync::Semaphore::new(1)),
            admission_observer: Some(admission_observer),
        }
    }
}

struct PlatformNotificationSubmitter {
    windows_aum_id: Option<crate::config::WindowsAumId>,
}

pub struct NativeNotifier {
    artwork: Option<Arc<dyn NotificationArtworkLoader>>,
    backend: Arc<dyn NativeNotificationBackend>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeArtworkMode {
    None,
    PrivatePngPath,
}

#[must_use]
pub const fn linux_replacement_id() -> Option<u32> {
    None
}

#[must_use]
pub const fn windows_notifications_supported(
    windows_aum_id: Option<&crate::config::WindowsAumId>,
) -> bool {
    windows_aum_id.is_some()
}

#[must_use]
pub const fn windows_artwork_mode() -> NativeArtworkMode {
    NativeArtworkMode::None
}

impl NativeNotifier {
    #[must_use]
    pub const fn artwork_mode() -> NativeArtworkMode {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            NativeArtworkMode::PrivatePngPath
        }
        #[cfg(target_os = "windows")]
        {
            windows_artwork_mode()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            NativeArtworkMode::None
        }
    }

    /// Constructs a notifier from cache state prepared before platform gates.
    /// Passing `None` keeps text notifications available without artwork.
    ///
    /// # Errors
    ///
    /// Returns a redacted error when the platform is unavailable or the
    /// bounded HTTP client cannot be initialized.
    pub fn from_prepared_cache(
        cache: Option<Arc<NotificationArtworkCache>>,
        windows_aum_id: Option<crate::config::WindowsAumId>,
    ) -> Result<Self, RuntimeNotifierError> {
        let platform_supported = current_platform_notifications_supported(windows_aum_id.as_ref());
        Self::from_prepared_services(
            cache,
            Self::artwork_mode(),
            platform_supported,
            || {
                let client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(2))
                    .read_timeout(Duration::from_secs(2))
                    .timeout(Duration::from_secs(2))
                    .build()
                    .map_err(|_| RuntimeNotifierError)?;
                Ok(Arc::new(BoundedNotificationArtworkLoader::new(Arc::new(
                    ReqwestNotificationArtworkTransport { client },
                ))) as Arc<dyn NotificationArtworkLoader>)
            },
            move |cache| {
                let submitter = Arc::new(PlatformNotificationSubmitter { windows_aum_id });
                match cache {
                    Some(cache) => {
                        Arc::new(CommittedNativeNotificationBackend::new(cache, submitter))
                            as Arc<dyn NativeNotificationBackend>
                    }
                    None => Arc::new(CommittedNativeNotificationBackend::new_text_only(submitter))
                        as Arc<dyn NativeNotificationBackend>,
                }
            },
        )
    }

    #[doc(hidden)]
    pub fn from_prepared_services<ArtworkFactory, BackendFactory>(
        cache: Option<Arc<NotificationArtworkCache>>,
        artwork_mode: NativeArtworkMode,
        platform_supported: bool,
        artwork_factory: ArtworkFactory,
        backend_factory: BackendFactory,
    ) -> Result<Self, RuntimeNotifierError>
    where
        ArtworkFactory:
            FnOnce() -> Result<Arc<dyn NotificationArtworkLoader>, RuntimeNotifierError>,
        BackendFactory:
            FnOnce(Option<Arc<NotificationArtworkCache>>) -> Arc<dyn NativeNotificationBackend>,
    {
        if !platform_supported {
            return Err(RuntimeNotifierError);
        }
        let artwork = if cache.is_some() && artwork_mode == NativeArtworkMode::PrivatePngPath {
            Some(artwork_factory()?)
        } else {
            None
        };
        Ok(Self {
            artwork,
            backend: backend_factory(cache),
        })
    }

    #[must_use]
    pub fn with_services(
        artwork: Arc<dyn NotificationArtworkLoader>,
        backend: Arc<dyn NativeNotificationBackend>,
    ) -> Self {
        Self {
            artwork: Some(artwork),
            backend,
        }
    }

    #[must_use]
    #[doc(hidden)]
    pub fn with_text_only_backend(backend: Arc<dyn NativeNotificationBackend>) -> Self {
        Self {
            artwork: None,
            backend,
        }
    }
}

const fn current_platform_notifications_supported(
    windows_aum_id: Option<&crate::config::WindowsAumId>,
) -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let _ = windows_aum_id;
        true
    }
    #[cfg(target_os = "windows")]
    {
        windows_notifications_supported(windows_aum_id)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = windows_aum_id;
        false
    }
}

#[async_trait]
impl NotificationArtworkTransport for ReqwestNotificationArtworkTransport {
    async fn fetch(
        &self,
        url: &url::Url,
        cancel: &CancellationToken,
    ) -> Result<NotificationArtworkStream, ArtworkTransportError> {
        if cancel.is_cancelled() {
            return Err(ArtworkTransportError);
        }
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ArtworkTransportError),
            response = self.client.get(url.clone()).send() => response.map_err(|_| ArtworkTransportError)?,
        }
        .error_for_status()
        .map_err(|_| ArtworkTransportError)?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_NOTIFICATION_ARTWORK_BYTES as u64)
        {
            return Err(ArtworkTransportError);
        }
        Ok(Box::pin(
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|_| ArtworkTransportError)),
        ))
    }
}

#[async_trait]
impl NotificationArtworkLoader for BoundedNotificationArtworkLoader {
    async fn load(
        &self,
        artwork: Option<&ArtworkUrl>,
        cancel: &CancellationToken,
    ) -> Option<PrivatePngAttachment> {
        if cancel.is_cancelled() {
            return None;
        }
        let artwork = artwork?;
        if let Some(observer) = &self.admission_observer {
            observer();
        }
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return None,
            permit = Arc::clone(&self.decode_admission).acquire_owned() => permit.ok()?,
        };
        let mut stream = tokio::select! {
            biased;
            () = cancel.cancelled() => return None,
            stream = self.transport.fetch(artwork.as_url(), cancel) => stream.ok()?,
        };
        let mut encoded = Vec::new();
        loop {
            let chunk = tokio::select! {
                biased;
                () = cancel.cancelled() => return None,
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            let chunk = chunk.ok()?;
            if encoded.len().saturating_add(chunk.len()) > MAX_NOTIFICATION_ARTWORK_BYTES {
                return None;
            }
            encoded.extend_from_slice(&chunk);
        }
        let decoder = Arc::clone(&self.decoder);
        run_owned_blocking(cancel.clone(), move || {
            let _permit = permit;
            decoder(&encoded)
        })
        .await
        .ok()
    }
}

#[async_trait]
impl RuntimeNotifier for NativeNotifier {
    async fn notify(
        &self,
        notification: NowPlayingNotification,
        cancel: CancellationToken,
    ) -> Result<(), RuntimeNotifierError> {
        if cancel.is_cancelled() {
            return Err(RuntimeNotifierError);
        }
        let body = notification_body(&notification);
        let attachment = match Self::artwork_mode() {
            NativeArtworkMode::PrivatePngPath => match &self.artwork {
                Some(artwork) => artwork.load(notification.artwork(), &cancel).await,
                None => None,
            },
            NativeArtworkMode::None => None,
        };
        if cancel.is_cancelled() {
            return Err(RuntimeNotifierError);
        }
        let request = NativeNotificationRequest {
            generation: notification.generation(),
            title: notification.title().to_owned(),
            body,
            artwork: attachment,
        };
        self.backend.send(request, cancel).await
    }
}

impl NativeNotificationSubmitter for PlatformNotificationSubmitter {
    fn submit(&self, request: &NativeSubmissionRequest) -> Result<(), RuntimeNotifierError> {
        let _ = &self.windows_aum_id;
        #[cfg(target_os = "linux")]
        {
            let mut native = notify_rust::Notification::new();
            native
                .appname("ytermusic")
                .summary(request.title())
                .body(request.body());
            if let Some(id) = linux_replacement_id() {
                native.id(id);
            }
            if let Some(path) = request.artwork_path()
                && let Some(path) = path.to_str()
            {
                native.image_path(path);
            }
            native.show().map(|_| ()).map_err(|_| RuntimeNotifierError)
        }
        #[cfg(target_os = "macos")]
        {
            let mut native = notify_rust::Notification::new();
            native
                .appname("ytermusic")
                .summary(request.title())
                .body(request.body())
                .id("ytermusic-now-playing");
            if let Some(path) = request.artwork_path()
                && let Some(path) = path.to_str()
            {
                native.image_path(path);
            }
            native.show().map(|_| ()).map_err(|_| RuntimeNotifierError)
        }
        #[cfg(target_os = "windows")]
        {
            let aum_id = self.windows_aum_id.as_ref().ok_or(RuntimeNotifierError)?;
            let manager = winrt_toast_reborn::ToastManager::new(aum_id.as_str());
            let mut toast = winrt_toast_reborn::Toast::new();
            toast
                .text1(request.title())
                .text2(request.body())
                .tag("now-playing")
                .group("ytermusic");
            manager.show(&toast).map_err(|_| RuntimeNotifierError)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            let _ = request;
            Err(RuntimeNotifierError)
        }
    }
}

fn notification_body(notification: &NowPlayingNotification) -> String {
    match (notification.creator(), notification.collection()) {
        (Some(creator), Some(collection)) => format!("{creator} · {collection}"),
        (Some(creator), None) => creator.to_owned(),
        (None, Some(collection)) => collection.to_owned(),
        (None, None) => "Now playing".to_owned(),
    }
}

pub struct NotificationWorker {
    requests: watch::Sender<Option<NowPlayingNotification>>,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
    diagnostic_count: Arc<AtomicUsize>,
}

impl NotificationWorker {
    #[must_use]
    pub fn new(notifier: Arc<dyn RuntimeNotifier>, timeout: Duration) -> Self {
        let (requests, receiver) = watch::channel(None);
        let shutdown = CancellationToken::new();
        let diagnostic_count = Arc::new(AtomicUsize::new(0));
        let task = tokio::spawn(run_notification_worker(
            notifier,
            timeout,
            receiver,
            shutdown.clone(),
            Arc::clone(&diagnostic_count),
        ));
        Self {
            requests,
            shutdown,
            task: Some(task),
            diagnostic_count,
        }
    }

    pub fn replace(&self, notification: NowPlayingNotification) {
        self.requests.send_replace(Some(notification));
    }

    #[must_use]
    pub fn diagnostic_count(&self) -> usize {
        self.diagnostic_count.load(Ordering::Acquire)
    }

    pub async fn shutdown(&mut self, deadline: tokio::time::Instant) {
        self.shutdown.cancel();
        if let Some(mut task) = self.task.take() {
            tokio::select! {
                biased;
                _ = &mut task => {}
                () = tokio::time::sleep_until(deadline) => {
                    task.abort();
                    let _ = task.await;
                }
            }
        }
    }
}

async fn run_notification_worker(
    notifier: Arc<dyn RuntimeNotifier>,
    timeout: Duration,
    mut requests: watch::Receiver<Option<NowPlayingNotification>>,
    shutdown: CancellationToken,
    diagnostic_count: Arc<AtomicUsize>,
) {
    let error_reported = AtomicBool::new(false);
    let mut pending = None;
    loop {
        if pending.is_some() && requests.has_changed().unwrap_or(false) {
            pending.clone_from(&requests.borrow_and_update());
        }
        let request = if let Some(request) = pending.take() {
            Some(request)
        } else {
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = requests.changed() => {
                    if changed.is_err() { return; }
                    requests.borrow_and_update().clone()
                }
            }
        };
        let Some(notification) = request else {
            continue;
        };
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let task_notifier = Arc::clone(&notifier);
        let mut operation =
            tokio::spawn(async move { task_notifier.notify(notification, task_cancel).await });
        let failed = tokio::select! {
            () = shutdown.cancelled() => {
                cancel.cancel();
                tokio::task::yield_now().await;
                if !operation.is_finished() {
                    operation.abort();
                }
                let _ = operation.await;
                return;
            }
            changed = requests.changed() => {
                if changed.is_err() {
                    cancel.cancel();
                    let _ = operation.await;
                    return;
                }
                pending.clone_from(&requests.borrow_and_update());
                cancel.cancel();
                tokio::task::yield_now().await;
                if !operation.is_finished() {
                    operation.abort();
                }
                let _ = operation.await;
                false
            }
            () = tokio::time::sleep(timeout) => {
                cancel.cancel();
                tokio::task::yield_now().await;
                if !operation.is_finished() {
                    operation.abort();
                }
                let _ = operation.await;
                true
            }
            result = &mut operation => !matches!(result, Ok(Ok(()))),
        };
        if failed && !error_reported.swap(true, Ordering::AcqRel) {
            diagnostic_count.fetch_add(1, Ordering::AcqRel);
            tracing::warn!("native notification is unavailable");
        }
    }
}
