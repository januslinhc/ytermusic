use std::{
    collections::VecDeque,
    fmt,
    hash::{Hash, Hasher},
    io::Cursor,
    pin::Pin,
    sync::{Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use image::{ImageReader, Limits, imageops::FilterType};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::Widget,
};
use thiserror::Error;
use tokio::{sync::Semaphore, time::Instant};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use url::Url;

use crate::{app::Generation, domain::ArtworkUrl};

use super::theme::ColorCapability;

pub const MAX_ENCODED_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_SOURCE_DIMENSION: u32 = 4096;
pub const MAX_SOURCE_PIXELS: u64 = 16 * 1024 * 1024;
pub const MAX_CELL_WIDTH: u16 = 256;
pub const MAX_CELL_HEIGHT: u16 = 128;
pub const MAX_OUTPUT_CELLS: usize = 32 * 1024;
pub const ARTWORK_LOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Production wide-mode artwork occupies 21 columns by 8 terminal rows.
///
/// Twenty-one columns retain the complete deterministic fallback label.
pub const PRODUCTION_ARTWORK_SIZE: CellSize = CellSize::new(21, 8);
pub const PRODUCTION_ARTWORK_CACHE_CAPACITY: usize = 32;
const MAX_DECODE_ALLOC_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENCODED_CHUNKS: usize = MAX_ENCODED_BYTES / 64;
const CHUNKS_PER_COOPERATIVE_YIELD: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CellSize {
    pub width: u16,
    pub height: u16,
}

impl CellSize {
    #[must_use]
    pub const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }

    #[must_use]
    fn cell_count(self) -> usize {
        usize::from(self.width) * usize::from(self.height)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Rgb {
    red: u8,
    green: u8,
    blue: u8,
}

impl Rgb {
    #[must_use]
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    #[must_use]
    pub const fn red(self) -> u8 {
        self.red
    }

    #[must_use]
    pub const fn green(self) -> u8 {
        self.green
    }

    #[must_use]
    pub const fn blue(self) -> u8 {
        self.blue
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ArtworkCell {
    foreground: Rgb,
    background: Rgb,
}

impl ArtworkCell {
    #[must_use]
    pub const fn glyph(self) -> char {
        '▀'
    }

    #[must_use]
    pub const fn foreground(self) -> Rgb {
        self.foreground
    }

    #[must_use]
    pub const fn background(self) -> Rgb {
        self.background
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ArtworkGrid {
    size: CellSize,
    cells: Vec<ArtworkCell>,
}

impl fmt::Debug for ArtworkGrid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtworkGrid")
            .field("size", &self.size)
            .field("cell_count", &self.cells.len())
            .finish_non_exhaustive()
    }
}

impl ArtworkGrid {
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.size.width
    }

    #[must_use]
    pub const fn height(&self) -> u16 {
        self.size.height
    }

    #[must_use]
    pub fn cells(&self) -> &[ArtworkCell] {
        &self.cells
    }

    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&ArtworkCell> {
        if x >= self.size.width || y >= self.size.height {
            return None;
        }
        let index = usize::from(y) * usize::from(self.size.width) + usize::from(x);
        self.cells.get(index)
    }

    const fn empty(size: CellSize) -> Self {
        Self {
            size,
            cells: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ArtworkError {
    #[error("encoded artwork exceeds the resource limit")]
    EncodedResourceLimit,
    #[error("decoded artwork exceeds the resource limit")]
    DecodedResourceLimit,
    #[error("requested artwork grid exceeds the resource limit")]
    OutputResourceLimit,
    #[error("artwork could not be decoded")]
    DecodeFailed,
}

/// Decodes image bytes into a terminal half-block grid of exactly `size`.
///
/// Nearest-neighbor sampling is used so the conversion is deterministic. RGBA
/// input is composited over black before each upper-half block is produced.
///
/// # Errors
///
/// Returns [`ArtworkError`] when the encoded input, decoded image, or requested
/// output exceeds a resource cap, or when the bytes are not a supported image.
pub fn decode_artwork(bytes: &[u8], size: CellSize) -> Result<ArtworkGrid, ArtworkError> {
    validate_output_size(size)?;
    if size.width == 0 || size.height == 0 {
        return Ok(ArtworkGrid::empty(size));
    }
    if bytes.len() > MAX_ENCODED_BYTES {
        return Err(ArtworkError::EncodedResourceLimit);
    }

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ArtworkError::DecodeFailed)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_DIMENSION);
    limits.max_image_height = Some(MAX_SOURCE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|error| match error {
        image::ImageError::Limits(_) => ArtworkError::DecodedResourceLimit,
        _ => ArtworkError::DecodeFailed,
    })?;
    let source_pixels = u64::from(decoded.width()) * u64::from(decoded.height());
    if source_pixels > MAX_SOURCE_PIXELS {
        return Err(ArtworkError::DecodedResourceLimit);
    }

    let pixel_height = u32::from(size.height)
        .checked_mul(2)
        .ok_or(ArtworkError::OutputResourceLimit)?;
    let resized = decoded
        .resize_exact(u32::from(size.width), pixel_height, FilterType::Nearest)
        .to_rgba8();
    let mut cells = Vec::with_capacity(size.cell_count());
    for cell_y in 0..size.height {
        let top_y = u32::from(cell_y) * 2;
        let bottom_y = top_y + 1;
        for x in 0..size.width {
            let foreground = composite_over_black(resized.get_pixel(u32::from(x), top_y).0);
            let background = composite_over_black(resized.get_pixel(u32::from(x), bottom_y).0);
            cells.push(ArtworkCell {
                foreground,
                background,
            });
        }
    }

    Ok(ArtworkGrid { size, cells })
}

/// Converts one tightly packed RGB24 frame into the same terminal half-block
/// grid used by static artwork.
///
/// # Errors
///
/// Returns [`ArtworkError::OutputResourceLimit`] for an invalid target size or
/// [`ArtworkError::DecodeFailed`] when `pixels` is not exactly one bounded
/// RGB24 frame of `width * (height * 2)` pixels.
pub fn decode_rgb_frame(pixels: &[u8], size: CellSize) -> Result<ArtworkGrid, ArtworkError> {
    validate_output_size(size)?;
    if size.width == 0 || size.height == 0 {
        return Err(ArtworkError::OutputResourceLimit);
    }
    let expected = size
        .cell_count()
        .checked_mul(2)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(ArtworkError::OutputResourceLimit)?;
    if pixels.len() != expected {
        return Err(ArtworkError::DecodeFailed);
    }
    let mut cells = Vec::with_capacity(size.cell_count());
    let row_bytes = usize::from(size.width) * 3;
    for cell_y in 0..size.height {
        let top_row = usize::from(cell_y) * 2 * row_bytes;
        let bottom_row = top_row + row_bytes;
        for x in 0..size.width {
            let offset = usize::from(x) * 3;
            let foreground = rgb_at(pixels, top_row + offset)?;
            let background = rgb_at(pixels, bottom_row + offset)?;
            cells.push(ArtworkCell {
                foreground,
                background,
            });
        }
    }
    Ok(ArtworkGrid { size, cells })
}

fn rgb_at(pixels: &[u8], offset: usize) -> Result<Rgb, ArtworkError> {
    let components = pixels
        .get(offset..offset.saturating_add(3))
        .ok_or(ArtworkError::DecodeFailed)?;
    match components {
        [red, green, blue] => Ok(Rgb::new(*red, *green, *blue)),
        _ => Err(ArtworkError::DecodeFailed),
    }
}

fn validate_output_size(size: CellSize) -> Result<(), ArtworkError> {
    if size.width > MAX_CELL_WIDTH
        || size.height > MAX_CELL_HEIGHT
        || size.cell_count() > MAX_OUTPUT_CELLS
    {
        return Err(ArtworkError::OutputResourceLimit);
    }
    Ok(())
}

fn composite_over_black([red, green, blue, alpha]: [u8; 4]) -> Rgb {
    const HALF: u32 = u8::MAX as u32 / 2;
    let alpha = u32::from(alpha);
    let component = |value: u8| {
        let composited = (u32::from(value) * alpha + HALF) / u32::from(u8::MAX);
        u8::try_from(composited).unwrap_or(u8::MAX)
    };
    Rgb::new(component(red), component(green), component(blue))
}

#[derive(Clone, Eq)]
pub struct ArtworkIdentity {
    canonical: String,
}

impl ArtworkIdentity {
    #[must_use]
    pub fn from_url(url: &Url) -> Self {
        Self {
            canonical: url.as_str().to_owned(),
        }
    }
}

impl fmt::Debug for ArtworkIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ArtworkIdentity([REDACTED])")
    }
}

impl Hash for ArtworkIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.canonical.hash(state);
    }
}

impl PartialEq for ArtworkIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    identity: ArtworkIdentity,
    size: CellSize,
}

#[derive(Clone)]
struct CacheEntry {
    key: CacheKey,
    grid: Arc<ArtworkGrid>,
}

pub struct ArtworkCache {
    capacity: usize,
    entries: VecDeque<CacheEntry>,
}

impl ArtworkCache {
    #[must_use]
    pub const fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: VecDeque::new(),
        }
    }

    #[must_use]
    pub fn get(&mut self, identity: &ArtworkIdentity, size: CellSize) -> Option<Arc<ArtworkGrid>> {
        let key = CacheKey {
            identity: identity.clone(),
            size,
        };
        let index = self.entries.iter().position(|entry| entry.key == key)?;
        let entry = self.entries.remove(index)?;
        let grid = Arc::clone(&entry.grid);
        self.entries.push_back(entry);
        Some(grid)
    }

    pub fn insert(&mut self, identity: ArtworkIdentity, size: CellSize, grid: Arc<ArtworkGrid>) {
        if self.capacity == 0 {
            return;
        }
        let key = CacheKey { identity, size };
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let _ = self.entries.remove(index);
        }
        while self.entries.len() >= self.capacity {
            let _ = self.entries.pop_front();
        }
        self.entries.push_back(CacheEntry { key, grid });
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl fmt::Debug for ArtworkCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtworkCache")
            .field("capacity", &self.capacity)
            .field("entry_count", &self.entries.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("artwork fetch failed")]
pub struct ArtworkFetchError;

impl ArtworkFetchError {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self
    }
}

#[async_trait]
pub trait ArtworkFetcher: Send + Sync {
    /// Opens a stream of encoded image chunks without coupling the renderer to
    /// network I/O.
    ///
    /// # Errors
    ///
    /// Returns [`ArtworkFetchError`] without embedding the requested URL.
    async fn fetch(&self, url: &Url) -> Result<ArtworkByteStream, ArtworkFetchError>;
}

pub type ArtworkByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, ArtworkFetchError>> + Send + 'static>>;

pub trait ArtworkDecoder: Send + Sync {
    /// Decodes and resizes one fully bounded encoded image.
    ///
    /// This synchronous method is always called on Tokio's blocking pool by
    /// [`CachedArtworkService`].
    ///
    /// # Errors
    ///
    /// Returns [`ArtworkError`] when the encoded or decoded image violates a
    /// resource bound or cannot be decoded.
    fn decode(&self, bytes: Vec<u8>, size: CellSize) -> Result<ArtworkGrid, ArtworkError>;
}

#[derive(Debug, Default)]
struct ImageArtworkDecoder;

impl ArtworkDecoder for ImageArtworkDecoder {
    fn decode(&self, bytes: Vec<u8>, size: CellSize) -> Result<ArtworkGrid, ArtworkError> {
        decode_artwork(&bytes, size)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FallbackReason {
    Fetch,
    Decode,
    UnsupportedColor,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtworkFallback {
    reason: FallbackReason,
}

impl ArtworkFallback {
    #[must_use]
    pub const fn icon(self) -> &'static str {
        "♪"
    }

    #[must_use]
    pub const fn metadata(self) -> &'static str {
        "Artwork unavailable"
    }
}

#[derive(Clone, Debug)]
pub enum ArtworkPresentation {
    Grid(Arc<ArtworkGrid>),
    Fallback(ArtworkFallback),
}

impl ArtworkPresentation {
    #[must_use]
    pub const fn unavailable() -> Self {
        fallback(FallbackReason::Unavailable)
    }

    #[must_use]
    pub const fn is_grid(&self) -> bool {
        matches!(self, Self::Grid(_))
    }
}

#[derive(Clone)]
struct ArtworkPresentationSlot {
    generation: Generation,
    identity: ArtworkIdentity,
    presentation: ArtworkPresentation,
}

/// One bounded, redacted presentation slot shared by the runtime and renderer.
pub struct ArtworkPresentationStore {
    current: RwLock<Option<ArtworkPresentationSlot>>,
}

impl ArtworkPresentationStore {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current: RwLock::new(None),
        }
    }

    pub fn request(&self, generation: Generation, url: &ArtworkUrl) {
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ArtworkPresentationSlot {
            generation,
            identity: ArtworkIdentity::from_url(url.as_url()),
            presentation: ArtworkPresentation::unavailable(),
        });
    }

    pub fn clear(&self) {
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub fn publish(
        &self,
        generation: Generation,
        url: &ArtworkUrl,
        presentation: ArtworkPresentation,
    ) -> bool {
        let identity = ArtworkIdentity::from_url(url.as_url());
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = current
            .as_mut()
            .filter(|slot| slot.generation == generation && slot.identity == identity)
        else {
            return false;
        };
        slot.presentation = presentation;
        true
    }

    #[must_use]
    pub fn presentation(
        &self,
        generation: Generation,
        url: &ArtworkUrl,
    ) -> Option<ArtworkPresentation> {
        let identity = ArtworkIdentity::from_url(url.as_url());
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|slot| slot.generation == generation && slot.identity == identity)
            .map(|slot| slot.presentation.clone())
    }
}

impl Default for ArtworkPresentationStore {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ArtworkPresentationStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let current = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("ArtworkPresentationStore")
            .field("generation", &current.as_ref().map(|slot| slot.generation))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ArtworkWidget<'a> {
    presentation: &'a ArtworkPresentation,
    capability: ColorCapability,
    fit_grid: bool,
}

impl<'a> ArtworkWidget<'a> {
    #[must_use]
    pub const fn new(presentation: &'a ArtworkPresentation, capability: ColorCapability) -> Self {
        Self {
            presentation,
            capability,
            fit_grid: false,
        }
    }

    /// Fits the complete static grid into the target area with bounded nearest
    /// neighbor sampling. Fallback text retains its normal clipping behavior.
    #[must_use]
    pub const fn new_fitted(
        presentation: &'a ArtworkPresentation,
        capability: ColorCapability,
    ) -> Self {
        Self {
            presentation,
            capability,
            fit_grid: true,
        }
    }
}

impl Widget for ArtworkWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        match self.presentation {
            ArtworkPresentation::Grid(grid) if self.capability != ColorCapability::Monochrome => {
                render_grid(grid, area, buffer, self.capability, self.fit_grid);
            }
            ArtworkPresentation::Grid(_) => {
                render_fallback(
                    ArtworkFallback {
                        reason: FallbackReason::UnsupportedColor,
                    },
                    area,
                    buffer,
                );
            }
            ArtworkPresentation::Fallback(fallback) => {
                render_fallback(*fallback, area, buffer);
            }
        }
    }
}

fn render_grid(
    grid: &ArtworkGrid,
    area: Rect,
    buffer: &mut Buffer,
    capability: ColorCapability,
    fit: bool,
) {
    let clipped = reset_area(area, buffer);
    for target_y in clipped.y..clipped.bottom() {
        for target_x in clipped.x..clipped.right() {
            let offset_x = target_x.saturating_sub(area.x);
            let offset_y = target_y.saturating_sub(area.y);
            let source_x = if fit {
                fitted_coordinate(offset_x, grid.width(), area.width)
            } else {
                offset_x
            };
            let source_y = if fit {
                fitted_coordinate(offset_y, grid.height(), area.height)
            } else {
                offset_y
            };
            let Some(source) = grid.cell(source_x, source_y) else {
                continue;
            };
            let Some(target) = buffer.cell_mut((target_x, target_y)) else {
                continue;
            };
            target.set_char(source.glyph()).set_style(
                Style::default()
                    .fg(terminal_color(source.foreground(), capability))
                    .bg(terminal_color(source.background(), capability)),
            );
        }
    }
}

/// Maps both target endpoints to the corresponding source endpoints and uses
/// rounded integer interpolation between them. A one-cell target samples the
/// lower of the two center cells when the source dimension is even.
fn fitted_coordinate(offset: u16, source_dimension: u16, target_dimension: u16) -> u16 {
    if source_dimension <= 1 {
        return 0;
    }
    let source_last = source_dimension - 1;
    if target_dimension <= 1 {
        return source_last / 2;
    }
    let target_last = u32::from(target_dimension - 1);
    let numerator = u32::from(offset)
        .saturating_mul(u32::from(source_last))
        .saturating_add(target_last / 2);
    u16::try_from(numerator / target_last).unwrap_or(source_last)
}

fn render_fallback(fallback: ArtworkFallback, area: Rect, buffer: &mut Buffer) {
    let clipped = reset_area(area, buffer);
    if clipped.height == 0 || area.y < clipped.y || area.y >= clipped.bottom() {
        return;
    }

    let text = format!("{} {}", fallback.icon(), fallback.metadata());
    let mut column = 0_usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if column.saturating_add(grapheme_width) > usize::from(area.width) {
            break;
        }
        let Ok(column_offset) = u16::try_from(column) else {
            break;
        };
        let target_x = area.x.saturating_add(column_offset);
        if target_x >= clipped.x
            && target_x < clipped.right()
            && let Some(target) = buffer.cell_mut((target_x, area.y))
        {
            target.set_symbol(grapheme).set_style(Style::default());
        }
        column = column.saturating_add(grapheme_width);
    }
}

fn reset_area(area: Rect, buffer: &mut Buffer) -> Rect {
    let clipped = area.intersection(buffer.area);
    for y in clipped.y..clipped.bottom() {
        for x in clipped.x..clipped.right() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
    clipped
}

fn terminal_color(rgb: Rgb, capability: ColorCapability) -> Color {
    match capability {
        ColorCapability::TrueColor => Color::Rgb(rgb.red(), rgb.green(), rgb.blue()),
        ColorCapability::Ansi256 => Color::Indexed(ansi256(rgb)),
        ColorCapability::Basic => basic_color(rgb),
        ColorCapability::Monochrome => Color::Reset,
    }
}

fn ansi256(rgb: Rgb) -> u8 {
    let scale = |component: u8| component / 51;
    16_u8
        .saturating_add(36_u8.saturating_mul(scale(rgb.red())))
        .saturating_add(6_u8.saturating_mul(scale(rgb.green())))
        .saturating_add(scale(rgb.blue()))
}

fn basic_color(rgb: Rgb) -> Color {
    let red = rgb.red() >= 128;
    let green = rgb.green() >= 128;
    let blue = rgb.blue() >= 128;
    let bright = rgb.red().max(rgb.green()).max(rgb.blue()) >= 192;
    match (red, green, blue, bright) {
        (false, false, false, false) => Color::Black,
        (true, false, false, false) => Color::Red,
        (false, true, false, false) => Color::Green,
        (true, true, false, false) => Color::Yellow,
        (false, false, true, false) => Color::Blue,
        (true, false, true, false) => Color::Magenta,
        (false, true, true, false) => Color::Cyan,
        (true, true, true, false) => Color::Gray,
        (true, false, false, true) => Color::LightRed,
        (false, true, false, true) => Color::LightGreen,
        (true, true, false, true) => Color::LightYellow,
        (false, false, true, true) => Color::LightBlue,
        (true, false, true, true) => Color::LightMagenta,
        (false, true, true, true) => Color::LightCyan,
        (true, true, true, true) => Color::White,
        (false, false, false, true) => Color::DarkGray,
    }
}

pub struct CachedArtworkService<F> {
    fetcher: F,
    cache: ArtworkCache,
    decoder: Arc<dyn ArtworkDecoder>,
    decode_permits: Arc<Semaphore>,
}

impl<F> CachedArtworkService<F>
where
    F: ArtworkFetcher,
{
    #[must_use]
    pub fn new(fetcher: F, capacity: usize) -> Self {
        Self::with_decoder(fetcher, capacity, ImageArtworkDecoder)
    }

    #[must_use]
    pub fn with_decoder<D>(fetcher: F, capacity: usize, decoder: D) -> Self
    where
        D: ArtworkDecoder + 'static,
    {
        Self {
            fetcher,
            cache: ArtworkCache::new(capacity),
            decoder: Arc::new(decoder),
            decode_permits: Arc::new(Semaphore::new(1)),
        }
    }

    pub async fn load(
        &mut self,
        url: &Url,
        size: CellSize,
        capability: ColorCapability,
    ) -> ArtworkPresentation {
        if validate_output_size(size).is_err() {
            return fallback(FallbackReason::Decode);
        }
        if size.width == 0 || size.height == 0 {
            return ArtworkPresentation::Grid(Arc::new(ArtworkGrid::empty(size)));
        }
        if capability == ColorCapability::Monochrome {
            return fallback(FallbackReason::UnsupportedColor);
        }

        let identity = ArtworkIdentity::from_url(url);
        if let Some(grid) = self.cache.get(&identity, size) {
            return ArtworkPresentation::Grid(grid);
        }

        let deadline = Instant::now() + ARTWORK_LOAD_TIMEOUT;
        let Ok(bytes) = fetch_bounded(&self.fetcher, url, deadline).await else {
            return fallback(FallbackReason::Fetch);
        };
        let Ok(grid) = decode_bounded(
            Arc::clone(&self.decoder),
            Arc::clone(&self.decode_permits),
            bytes,
            size,
            deadline,
        )
        .await
        else {
            return fallback(FallbackReason::Decode);
        };
        let grid = Arc::new(grid);
        self.cache.insert(identity, size, Arc::clone(&grid));
        ArtworkPresentation::Grid(grid)
    }
}

async fn fetch_bounded<F>(
    fetcher: &F,
    url: &Url,
    deadline: Instant,
) -> Result<Vec<u8>, ArtworkFetchError>
where
    F: ArtworkFetcher,
{
    tokio::time::timeout_at(deadline, async {
        let mut stream = fetcher.fetch(url).await?;
        let mut bytes = Vec::new();
        let mut chunk_count = 0_usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            chunk_count = chunk_count
                .checked_add(1)
                .ok_or_else(ArtworkFetchError::unavailable)?;
            if chunk_count > MAX_ENCODED_CHUNKS || Instant::now() >= deadline {
                return Err(ArtworkFetchError::unavailable());
            }
            let Some(next_len) = bytes.len().checked_add(chunk.len()) else {
                return Err(ArtworkFetchError::unavailable());
            };
            if next_len > MAX_ENCODED_BYTES {
                return Err(ArtworkFetchError::unavailable());
            }
            bytes
                .try_reserve_exact(chunk.len())
                .map_err(|_| ArtworkFetchError::unavailable())?;
            bytes.extend_from_slice(&chunk);
            if chunk_count.is_multiple_of(CHUNKS_PER_COOPERATIVE_YIELD) {
                tokio::task::yield_now().await;
            }
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ArtworkFetchError::unavailable())?
}

async fn decode_bounded(
    decoder: Arc<dyn ArtworkDecoder>,
    permits: Arc<Semaphore>,
    bytes: Vec<u8>,
    size: CellSize,
    deadline: Instant,
) -> Result<ArtworkGrid, ArtworkError> {
    let permit = tokio::time::timeout_at(deadline, permits.acquire_owned())
        .await
        .map_err(|_| ArtworkError::DecodeFailed)?
        .map_err(|_| ArtworkError::DecodeFailed)?;
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decoder.decode(bytes, size)
    });
    tokio::time::timeout_at(deadline, worker)
        .await
        .map_err(|_| ArtworkError::DecodeFailed)?
        .map_err(|_| ArtworkError::DecodeFailed)?
}

impl<F> fmt::Debug for CachedArtworkService<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachedArtworkService")
            .field("cache", &self.cache)
            .finish_non_exhaustive()
    }
}

const fn fallback(reason: FallbackReason) -> ArtworkPresentation {
    ArtworkPresentation::Fallback(ArtworkFallback { reason })
}
