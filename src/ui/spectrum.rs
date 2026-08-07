use std::{
    ffi::OsString,
    fmt, io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt as _},
    process::Command,
    sync::watch,
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{app::Generation, domain::MediaId, resolver::AnalysisStreamUrl};

pub const ANALYSIS_SAMPLE_RATE: u16 = 8_000;
pub const FFT_SIZE: usize = 512;
pub const MAX_SPECTRUM_BANDS: usize = 64;
pub const MAX_SPECTRUM_LEVEL: u8 = 24;

const MAX_ROWS: u8 = 3;
const MIN_FREQUENCY_HZ: f32 = 40.0;
const NORMALIZATION_FLOOR_DB: f32 = -60.0;
const NORMALIZATION_CEILING_DB: f32 = 0.0;
const ATTACK_FACTOR: f32 = 0.85;
const DECAY_FACTOR: f32 = 0.25;
pub(crate) const MAX_SPECTRUM_FPS: u8 = 30;
const MAX_ANALYSIS_SECONDS: u64 = 4 * 60 * 60;
const MAX_FFMPEG_ALLOC_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FFMPEG_PROBE_BYTES: u64 = 1024 * 1024;
const MAX_FFMPEG_ANALYZE_MICROS: u64 = 5_000_000;
const FFMPEG_IO_TIMEOUT_MICROS: u64 = 10_000_000;
const CHILD_REAP_GRACE: Duration = Duration::from_millis(50);
const DECODER_REAP_GRACE: Duration = Duration::from_millis(200);
const MAX_FFMPEG_PROCESS_RUNTIME: Duration = Duration::from_secs(MAX_ANALYSIS_SECONDS + 300);
const MIN_FFMPEG_PROCESS_RUNTIME: Duration = Duration::from_millis(1);
const PCM_FRAME_BYTES: usize = FFT_SIZE * size_of::<f32>();
const MAX_NON_FINITE_SAMPLES: usize = FFT_SIZE / 8;

pub(crate) const fn effective_spectrum_fps(max_fps: u8) -> u8 {
    if max_fps < 1 {
        1
    } else if max_fps > MAX_SPECTRUM_FPS {
        MAX_SPECTRUM_FPS
    } else {
        max_fps
    }
}

const fn max_pcm_frame_count() -> u64 {
    let samples_per_frame = FFT_SIZE as u64;
    let frames_per_second = (ANALYSIS_SAMPLE_RATE as u64).div_ceil(samples_per_frame);
    frames_per_second * MAX_ANALYSIS_SECONDS
}

pub struct SpectrumProcessor {
    fft: Arc<dyn Fft<f32>>,
    window: Box<[f32]>,
    buffer: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    bin_bands: Box<[usize]>,
    band_peaks: Box<[f32]>,
    previous_levels: Box<[f32]>,
    inverse_fft_size: f32,
    amplitude_scale: f32,
    nyquist_amplitude_scale: f32,
}

impl SpectrumProcessor {
    #[must_use]
    pub fn new(bands: usize) -> Option<Self> {
        if !(1..=MAX_SPECTRUM_BANDS).contains(&bands) {
            return None;
        }

        let mut planner = FftPlanner::new();
        let fft = planner.plan_fft_forward(FFT_SIZE);
        let fft_size = f32::from(u16::try_from(FFT_SIZE).ok()?);
        let phase_step = 2.0 * std::f32::consts::PI / (fft_size - 1.0);
        let window = (0..FFT_SIZE)
            .scan(0.0_f32, |phase, _| {
                let coefficient = 0.5 * (1.0 - phase.cos());
                *phase += phase_step;
                Some(coefficient)
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let window_sum = window.iter().sum::<f32>();
        let sample_rate = f32::from(ANALYSIS_SAMPLE_RATE);
        let nyquist_hz = sample_rate / 2.0;
        let band_count = f32::from(u8::try_from(bands).ok()?);
        let boundary_ratio = ((nyquist_hz / MIN_FREQUENCY_HZ).ln() / band_count).exp();
        let frequency_step = sample_rate / fft_size;
        let positive_bin_count = FFT_SIZE / 2;
        let mut boundary_bin = 0;
        let mut boundary_frequency_hz = frequency_step;
        let mut next_boundary_hz = MIN_FREQUENCY_HZ * boundary_ratio;
        let mut boundaries = Vec::with_capacity(bands + 1);
        boundaries.push(0);
        for boundary in 1..bands {
            while boundary_bin < positive_bin_count && boundary_frequency_hz < next_boundary_hz {
                boundary_bin += 1;
                boundary_frequency_hz += frequency_step;
            }
            let minimum = boundaries[boundary - 1] + 1;
            let maximum = positive_bin_count - (bands - boundary);
            boundary_bin = boundary_bin.clamp(minimum, maximum);
            boundaries.push(boundary_bin);
            next_boundary_hz *= boundary_ratio;
        }
        boundaries.push(positive_bin_count);
        let mut bin_bands = Vec::with_capacity(positive_bin_count);
        for (band, boundary) in boundaries.windows(2).enumerate() {
            bin_bands.extend(std::iter::repeat_n(band, boundary[1] - boundary[0]));
        }
        let scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];

        Some(Self {
            fft,
            window,
            buffer: vec![Complex::new(0.0, 0.0); FFT_SIZE],
            scratch,
            bin_bands: bin_bands.into_boxed_slice(),
            band_peaks: vec![0.0; bands].into_boxed_slice(),
            previous_levels: vec![0.0; bands].into_boxed_slice(),
            inverse_fft_size: fft_size.recip(),
            amplitude_scale: 2.0 / window_sum,
            nyquist_amplitude_scale: window_sum.recip(),
        })
    }

    #[must_use]
    pub fn process(&mut self, samples: &[f32]) -> Option<SpectrumFrame> {
        if samples.len() != FFT_SIZE {
            return None;
        }

        let mean = samples
            .iter()
            .copied()
            .map(|sample| if sample.is_finite() { sample } else { 0.0 })
            .sum::<f32>()
            * self.inverse_fft_size;
        for ((bin, sample), window) in self
            .buffer
            .iter_mut()
            .zip(samples.iter().copied())
            .zip(self.window.iter().copied())
        {
            let finite_sample = if sample.is_finite() { sample } else { 0.0 };
            *bin = Complex::new((finite_sample - mean) * window, 0.0);
        }
        self.fft
            .process_with_scratch(&mut self.buffer, &mut self.scratch);
        self.band_peaks.fill(0.0);

        let band_count = self.band_peaks.len();

        for (index, (bin, band)) in self
            .buffer
            .iter()
            .skip(1)
            .take(FFT_SIZE / 2)
            .zip(self.bin_bands.iter().copied())
            .enumerate()
        {
            let scale = if index + 1 == FFT_SIZE / 2 {
                self.nyquist_amplitude_scale
            } else {
                self.amplitude_scale
            };
            let amplitude = bin.norm() * scale;
            self.band_peaks[band] = self.band_peaks[band].max(amplitude);
        }

        let mut levels = Vec::with_capacity(band_count);
        for (peak, previous) in self.band_peaks.iter().zip(self.previous_levels.iter_mut()) {
            let normalized = normalize_amplitude(*peak) * f32::from(MAX_SPECTRUM_LEVEL);
            let smoothing = if normalized > *previous {
                ATTACK_FACTOR
            } else {
                DECAY_FACTOR
            };
            *previous += (normalized - *previous) * smoothing;
            let rounded = previous.round().clamp(0.0, f32::from(MAX_SPECTRUM_LEVEL));
            let level = (0..=MAX_SPECTRUM_LEVEL)
                .rev()
                .find(|candidate| rounded >= f32::from(*candidate))
                .unwrap_or(0);
            levels.push(level);
        }

        SpectrumFrame::new(levels.into_boxed_slice())
    }
}

impl fmt::Debug for SpectrumProcessor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpectrumProcessor")
            .field("band_count", &self.band_peaks.len())
            .finish_non_exhaustive()
    }
}

fn normalize_amplitude(amplitude: f32) -> f32 {
    if amplitude <= 0.0 || !amplitude.is_finite() {
        return 0.0;
    }
    let decibels = 20.0 * amplitude.log10();
    ((decibels - NORMALIZATION_FLOOR_DB) / (NORMALIZATION_CEILING_DB - NORMALIZATION_FLOOR_DB))
        .clamp(0.0, 1.0)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SpectrumTarget {
    bands: u16,
    rows: u8,
}

impl SpectrumTarget {
    #[must_use]
    pub const fn new(bands: u16, rows: u8) -> Option<Self> {
        if bands >= 1 && bands as usize <= MAX_SPECTRUM_BANDS && rows >= 1 && rows <= MAX_ROWS {
            Some(Self { bands, rows })
        } else {
            None
        }
    }

    #[must_use]
    pub const fn bands(self) -> u16 {
        self.bands
    }

    #[must_use]
    pub const fn rows(self) -> u8 {
        self.rows
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct SpectrumKey {
    generation: Generation,
    media_id: MediaId,
    target: SpectrumTarget,
}

impl SpectrumKey {
    #[must_use]
    pub const fn new(generation: Generation, media_id: MediaId, target: SpectrumTarget) -> Self {
        Self {
            generation,
            media_id,
            target,
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn media_id(&self) -> &MediaId {
        &self.media_id
    }

    #[must_use]
    pub const fn target(&self) -> SpectrumTarget {
        self.target
    }
}

impl fmt::Debug for SpectrumKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpectrumKey")
            .field("band_count", &self.target.bands)
            .field("row_count", &self.target.rows)
            .finish_non_exhaustive()
    }
}

#[derive(Eq, PartialEq)]
pub struct SpectrumFrame {
    levels: Box<[u8]>,
}

impl SpectrumFrame {
    #[must_use]
    pub fn new(levels: Box<[u8]>) -> Option<Self> {
        (!levels.is_empty()
            && levels.len() <= MAX_SPECTRUM_BANDS
            && levels.iter().all(|level| *level <= MAX_SPECTRUM_LEVEL))
        .then_some(Self { levels })
    }

    #[must_use]
    pub fn levels(&self) -> &[u8] {
        &self.levels
    }
}

impl fmt::Debug for SpectrumFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpectrumFrame")
            .field("level_count", &self.levels.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SpectrumPresentation {
    frame: Option<Arc<SpectrumFrame>>,
    paused: bool,
    failed: bool,
}

impl SpectrumPresentation {
    #[must_use]
    pub const fn quiet() -> Self {
        Self {
            frame: None,
            paused: false,
            failed: false,
        }
    }

    #[must_use]
    pub const fn frame(&self) -> Option<&Arc<SpectrumFrame>> {
        self.frame.as_ref()
    }

    #[must_use]
    pub const fn paused(&self) -> bool {
        self.paused
    }

    #[must_use]
    pub const fn failed(&self) -> bool {
        self.failed
    }
}

impl fmt::Debug for SpectrumPresentation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpectrumPresentation")
            .field(
                "band_count",
                &self.frame.as_ref().map(|frame| frame.levels.len()),
            )
            .field("paused", &self.paused)
            .field("failed", &self.failed)
            .finish()
    }
}

#[derive(Clone)]
struct SpectrumSlot {
    key: SpectrumKey,
    lease: SpectrumLease,
    frame: Option<Arc<SpectrumFrame>>,
    paused: bool,
    failed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SpectrumLease(u64);

#[derive(Clone)]
pub struct SpectrumRun {
    key: SpectrumKey,
    lease: SpectrumLease,
}

impl fmt::Debug for SpectrumRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpectrumRun")
            .field("band_count", &self.key.target.bands)
            .field("row_count", &self.key.target.rows)
            .finish_non_exhaustive()
    }
}

pub struct SpectrumFrameStore {
    current: RwLock<Option<SpectrumSlot>>,
    redraw: watch::Sender<u64>,
    next_lease: AtomicU64,
}

impl Default for SpectrumFrameStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SpectrumFrameStore {
    #[must_use]
    pub fn new() -> Self {
        let (redraw, _) = watch::channel(0);
        Self {
            current: RwLock::new(None),
            redraw,
            next_lease: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn subscribe_redraw(&self) -> watch::Receiver<u64> {
        self.redraw.subscribe()
    }

    pub fn request(&self, key: SpectrumKey) -> Option<SpectrumRun> {
        let run_key = key.clone();
        self.request_with_lease(key).map(|lease| SpectrumRun {
            key: run_key,
            lease,
        })
    }

    fn request_with_lease(&self, key: SpectrumKey) -> Option<SpectrumLease> {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(lease) = self.allocate_lease() else {
            *current = None;
            drop(current);
            self.notify_redraw();
            return None;
        };
        *current = Some(SpectrumSlot {
            key,
            lease,
            frame: None,
            paused: false,
            failed: false,
        });
        Some(lease)
    }

    pub fn publish(&self, run: &SpectrumRun, frame: Arc<SpectrumFrame>) -> bool {
        self.publish_with_lease(&run.key, run.lease, frame)
    }

    fn publish_with_lease(
        &self,
        key: &SpectrumKey,
        lease: SpectrumLease,
        frame: Arc<SpectrumFrame>,
    ) -> bool {
        if frame.levels.len() != usize::from(key.target.bands) {
            return false;
        }
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = current
            .as_mut()
            .filter(|slot| slot.key == *key && slot.lease == lease && !slot.paused && !slot.failed)
        else {
            return false;
        };
        slot.frame = Some(frame);
        drop(current);
        self.notify_redraw();
        true
    }

    #[must_use]
    pub fn presentation(&self, key: &SpectrumKey) -> SpectrumPresentation {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|slot| slot.key == *key)
            .map_or_else(SpectrumPresentation::quiet, |slot| SpectrumPresentation {
                frame: slot.frame.clone(),
                paused: slot.paused,
                failed: slot.failed,
            })
    }

    pub fn pause(&self, run: &SpectrumRun) -> bool {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = current
            .as_mut()
            .filter(|slot| slot.key == run.key && slot.lease == run.lease && !slot.failed)
        else {
            return false;
        };
        slot.paused = true;
        drop(current);
        self.notify_redraw();
        true
    }

    pub fn resume(&self, run: &SpectrumRun) -> Option<SpectrumRun> {
        self.resume_with_new_lease(run).map(|lease| SpectrumRun {
            key: run.key.clone(),
            lease,
        })
    }

    fn resume_with_new_lease(&self, run: &SpectrumRun) -> Option<SpectrumLease> {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !current.as_ref().is_some_and(|slot| {
            slot.key == run.key && slot.lease == run.lease && slot.paused && !slot.failed
        }) {
            return None;
        }
        let Some(lease) = self.allocate_lease() else {
            *current = None;
            drop(current);
            self.notify_redraw();
            return None;
        };
        let slot = current.as_mut()?;
        slot.lease = lease;
        slot.paused = false;
        drop(current);
        self.notify_redraw();
        Some(lease)
    }

    pub fn fail(&self, run: &SpectrumRun) -> bool {
        self.fail_with_lease(&run.key, run.lease)
    }

    fn fail_with_lease(&self, key: &SpectrumKey, lease: SpectrumLease) -> bool {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(slot) = current
            .as_mut()
            .filter(|slot| slot.key == *key && slot.lease == lease && !slot.paused)
        else {
            return false;
        };
        slot.frame = None;
        slot.paused = false;
        slot.failed = true;
        drop(current);
        self.notify_redraw();
        true
    }

    pub fn clear(&self) {
        let removed = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .is_some();
        if removed {
            self.notify_redraw();
        }
    }

    fn allocate_lease(&self) -> Option<SpectrumLease> {
        self.next_lease
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |lease| {
                lease.checked_add(1)
            })
            .ok()
            .and_then(|lease| lease.checked_add(1))
            .map(SpectrumLease)
    }

    fn notify_redraw(&self) {
        self.redraw.send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
    }
}

impl fmt::Debug for SpectrumFrameStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let current = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("SpectrumFrameStore")
            .field(
                "band_count",
                &current.as_ref().map(|slot| slot.key.target.bands),
            )
            .field(
                "row_count",
                &current.as_ref().map(|slot| slot.key.target.rows),
            )
            .field(
                "has_frame",
                &current.as_ref().is_some_and(|slot| slot.frame.is_some()),
            )
            .field("paused", &current.as_ref().is_some_and(|slot| slot.paused))
            .field("failed", &current.as_ref().is_some_and(|slot| slot.failed))
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct SpectrumRequest {
    key: SpectrumKey,
    stream_url: AnalysisStreamUrl,
    max_fps: u8,
    start_ms: u64,
}

impl SpectrumRequest {
    #[must_use]
    pub const fn new(key: SpectrumKey, stream_url: AnalysisStreamUrl) -> Self {
        Self {
            key,
            stream_url,
            max_fps: MAX_SPECTRUM_FPS,
            start_ms: 0,
        }
    }

    #[must_use]
    pub const fn key(&self) -> &SpectrumKey {
        &self.key
    }

    #[must_use]
    pub fn matches_stream_url(&self, stream_url: &AnalysisStreamUrl) -> bool {
        &self.stream_url == stream_url
    }

    #[must_use]
    pub const fn start_ms(&self) -> u64 {
        self.start_ms
    }

    fn same_source(&self, other: &Self) -> bool {
        self.key == other.key && self.stream_url == other.stream_url
    }

    #[must_use]
    pub(crate) fn with_max_fps(mut self, max_fps: u8) -> Self {
        self.max_fps = effective_spectrum_fps(max_fps);
        self
    }

    #[must_use]
    pub(crate) const fn with_start_ms(mut self, start_ms: u64) -> Self {
        self.start_ms = start_ms;
        self
    }
}

impl fmt::Debug for SpectrumRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpectrumRequest")
            .field("band_count", &self.key.target.bands)
            .field("row_count", &self.key.target.rows)
            .field("stream_url", &"[REDACTED]")
            .field("max_fps", &self.max_fps)
            .field("start_ms", &self.start_ms)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SpectrumError {
    #[error("spectrum decoder is unavailable")]
    Unavailable,
    #[error("spectrum analysis violated a resource limit")]
    ResourceLimit,
    #[error("spectrum decoding failed")]
    DecodeFailed,
}

pub type SpectrumFrameOutput = Result<Arc<SpectrumFrame>, SpectrumError>;

#[async_trait]
pub trait SpectrumDecoder: Send + Sync {
    /// Decodes bounded PCM and replaces `output` with the newest spectrum frame.
    /// Implementations must stop and reap owned resources when `cancel` fires.
    async fn decode(
        &self,
        request: SpectrumRequest,
        output: watch::Sender<Option<SpectrumFrameOutput>>,
        cancel: CancellationToken,
    ) -> Result<(), SpectrumError>;
}

#[async_trait]
pub trait SpectrumPacer: Send + Sync {
    async fn wait(&self, duration: Duration);
}

#[derive(Debug, Default)]
pub struct TokioSpectrumPacer;

#[async_trait]
impl SpectrumPacer for TokioSpectrumPacer {
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

struct ActiveSpectrum {
    run: SpectrumRun,
    request: SpectrumRequest,
    cancel: CancellationToken,
    decoder: JoinHandle<()>,
    publisher: JoinHandle<()>,
}

impl ActiveSpectrum {
    fn retire(self) -> JoinHandle<()> {
        self.cancel.cancel();
        tokio::spawn(async move {
            let mut decoder = self.decoder;
            let mut publisher = self.publisher;
            if tokio::time::timeout(DECODER_REAP_GRACE, async {
                let _ = tokio::join!(&mut decoder, &mut publisher);
            })
            .await
            .is_err()
            {
                decoder.abort();
                publisher.abort();
                let _ = decoder.await;
                let _ = publisher.await;
            }
        })
    }
}

pub struct SpectrumWorker {
    decoder: Arc<dyn SpectrumDecoder>,
    pacer: Arc<dyn SpectrumPacer>,
    store: Arc<SpectrumFrameStore>,
    max_fps: u8,
    active: Option<ActiveSpectrum>,
    paused: Option<(SpectrumRequest, SpectrumRun)>,
    retiring: Vec<JoinHandle<()>>,
}

impl SpectrumWorker {
    #[must_use]
    pub fn spawn(
        decoder: Arc<dyn SpectrumDecoder>,
        pacer: Arc<dyn SpectrumPacer>,
        store: Arc<SpectrumFrameStore>,
        max_fps: u8,
    ) -> Self {
        Self {
            decoder,
            pacer,
            store,
            max_fps: effective_spectrum_fps(max_fps),
            active: None,
            paused: None,
            retiring: Vec::new(),
        }
    }

    pub fn replace(&mut self, request: SpectrumRequest) {
        self.retire_active();
        self.paused = None;
        self.prune_retired();
        let request = request.with_max_fps(self.max_fps);
        let Some(run) = self.store.request(request.key.clone()) else {
            return;
        };
        self.start(request, run);
    }

    fn start(&mut self, request: SpectrumRequest, run: SpectrumRun) {
        let cancel = CancellationToken::new();
        let (output, output_rx) = watch::channel(None);
        let decoder = Arc::clone(&self.decoder);
        let decoder_cancel = cancel.clone();
        let decoder_request = request.clone();
        let error_output = output.clone();
        let decoder_task = tokio::spawn(async move {
            if let Err(error) = decoder
                .decode(decoder_request, output, decoder_cancel)
                .await
            {
                error_output.send_replace(Some(Err(error)));
            }
        });
        let publisher = tokio::spawn(publish_latest_spectrum_frames(
            run.clone(),
            Arc::clone(&self.store),
            Arc::clone(&self.pacer),
            output_rx,
            cancel.clone(),
            self.max_fps,
        ));
        self.active = Some(ActiveSpectrum {
            run,
            request,
            cancel,
            decoder: decoder_task,
            publisher,
        });
    }

    pub fn pause(&mut self) {
        self.prune_retired();
        if let Some(active) = self.active.take() {
            if self.store.pause(&active.run) {
                self.paused = Some((active.request.clone(), active.run.clone()));
            }
            self.retiring.push(active.retire());
        }
    }

    pub fn resume(&mut self, position_ms: u64) {
        self.prune_retired();
        if self.active.is_some() {
            return;
        }
        if let Some((request, old_run)) = self.paused.take() {
            let request = request.with_start_ms(position_ms);
            if let Some(run) = self.store.resume(&old_run) {
                self.start(request, run);
            }
        }
    }

    pub fn seek(&mut self, position_ms: u64) {
        if let Some(request) = self.active.as_ref().map(|active| active.request.clone()) {
            self.replace(request.with_start_ms(position_ms));
        } else if let Some((request, _run)) = self.paused.as_mut() {
            request.start_ms = position_ms;
        }
    }

    #[must_use]
    pub fn active_key(&self) -> Option<&SpectrumKey> {
        self.active
            .as_ref()
            .map(|active| &active.request.key)
            .or_else(|| self.paused.as_ref().map(|(request, _)| &request.key))
    }

    #[must_use]
    pub(crate) fn active_request_matches(&self, request: &SpectrumRequest) -> bool {
        self.active
            .as_ref()
            .map(|active| &active.request)
            .or_else(|| self.paused.as_ref().map(|(request, _)| request))
            .is_some_and(|active| active.same_source(request))
    }

    #[must_use]
    pub fn redraw_receiver(&self) -> watch::Receiver<u64> {
        self.store.subscribe_redraw()
    }

    pub fn clear(&mut self) {
        self.retire_active();
        self.paused = None;
        self.store.clear();
    }

    pub async fn shutdown(&mut self) {
        self.clear();
        for task in self.retiring.drain(..) {
            let _ = task.await;
        }
    }

    fn retire_active(&mut self) {
        if let Some(active) = self.active.take() {
            self.retiring.push(active.retire());
        }
    }

    fn prune_retired(&mut self) {
        self.retiring.retain(|task| !task.is_finished());
    }
}

impl Drop for SpectrumWorker {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            active.cancel.cancel();
            // Detached tasks retain decoder/process ownership until bounded cleanup finishes.
            drop(active);
        }
    }
}

async fn publish_latest_spectrum_frames(
    run: SpectrumRun,
    store: Arc<SpectrumFrameStore>,
    pacer: Arc<dyn SpectrumPacer>,
    mut output: watch::Receiver<Option<SpectrumFrameOutput>>,
    cancel: CancellationToken,
    max_fps: u8,
) {
    let interval = Duration::from_secs_f64(1.0 / f64::from(max_fps));
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            changed = output.changed() => {
                if changed.is_err() { return; }
                let latest = output.borrow_and_update().clone();
                match latest {
                    Some(Ok(frame)) => {
                        if store.publish(&run, frame) { pacer.wait(interval).await; }
                    }
                    Some(Err(_)) => { let _ = store.fail(&run); return; }
                    None => {}
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct FfmpegSpectrumDecoder {
    executable: PathBuf,
    process_timeout: Duration,
    launcher: Arc<dyn SpectrumProcessLauncher>,
}

impl FfmpegSpectrumDecoder {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            process_timeout: MAX_FFMPEG_PROCESS_RUNTIME,
            launcher: Arc::new(TokioSpectrumProcessLauncher),
        }
    }

    #[must_use]
    pub fn with_process_timeout(mut self, timeout: Duration) -> Self {
        self.process_timeout =
            timeout.clamp(MIN_FFMPEG_PROCESS_RUNTIME, MAX_FFMPEG_PROCESS_RUNTIME);
        self
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    #[cfg(test)]
    fn with_launcher(mut self, launcher: Arc<dyn SpectrumProcessLauncher>) -> Self {
        self.launcher = launcher;
        self
    }
}

impl fmt::Debug for FfmpegSpectrumDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FfmpegSpectrumDecoder { executable: [REDACTED] }")
    }
}

#[async_trait]
impl SpectrumDecoder for FfmpegSpectrumDecoder {
    async fn decode(
        &self,
        request: SpectrumRequest,
        output: watch::Sender<Option<SpectrumFrameOutput>>,
        cancel: CancellationToken,
    ) -> Result<(), SpectrumError> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let mut processor = SpectrumProcessor::new(usize::from(request.key.target.bands))
            .ok_or(SpectrumError::ResourceLimit)?;
        if cancel.is_cancelled() {
            return Ok(());
        }
        let mut child = self
            .launcher
            .spawn(&self.executable, &ffmpeg_arguments(&request))
            .map_err(|_| SpectrumError::Unavailable)?;
        if cancel.is_cancelled() {
            kill_and_wait_spectrum(child).await?;
            return Ok(());
        }
        let Some(mut stdout) = child.take_stdout() else {
            kill_and_wait_spectrum(child).await?;
            return Err(SpectrumError::Unavailable);
        };
        let deadline = tokio::time::Instant::now() + self.process_timeout;
        let frame_limit = max_pcm_frame_count();
        let mut bytes = [0_u8; PCM_FRAME_BYTES];
        let mut decoded = 0_u64;
        loop {
            if decoded >= frame_limit {
                kill_and_wait_spectrum(child).await?;
                return Err(SpectrumError::ResourceLimit);
            }
            let read = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    kill_and_wait_spectrum(child).await?;
                    return Ok(());
                }
                () = tokio::time::sleep_until(deadline) => {
                    kill_and_wait_spectrum(child).await?;
                    return Err(SpectrumError::ResourceLimit);
                }
                result = read_pcm_frame(&mut stdout, &mut bytes) => result,
            };
            let has_frame = match read {
                Ok(has_frame) => has_frame,
                Err(error) => {
                    kill_and_wait_spectrum(child).await?;
                    return Err(error);
                }
            };
            if !has_frame {
                break;
            }
            let samples = match decode_pcm_samples(&bytes) {
                Ok(samples) => samples,
                Err(error) => {
                    kill_and_wait_spectrum(child).await?;
                    return Err(error);
                }
            };
            let Some(frame) = processor.process(&samples) else {
                kill_and_wait_spectrum(child).await?;
                return Err(SpectrumError::DecodeFailed);
            };
            output.send_replace(Some(Ok(Arc::new(frame))));
            decoded += 1;
        }
        drop(stdout);
        if decoded == 0 {
            kill_and_wait_spectrum(child).await?;
            return Err(SpectrumError::DecodeFailed);
        }
        let status = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                kill_and_wait_spectrum(child).await?;
                return Ok(());
            }
            () = tokio::time::sleep_until(deadline) => {
                kill_and_wait_spectrum(child).await?;
                return Err(SpectrumError::ResourceLimit);
            }
            result = child.wait() => result.map_err(|_| SpectrumError::DecodeFailed)?,
        };
        status.then_some(()).ok_or(SpectrumError::DecodeFailed)
    }
}

async fn read_pcm_frame(
    stdout: &mut SpectrumProcessStdout,
    frame: &mut [u8; PCM_FRAME_BYTES],
) -> Result<bool, SpectrumError> {
    let mut filled = 0;
    while filled < frame.len() {
        match stdout.read(&mut frame[filled..]).await {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) | Err(_) => return Err(SpectrumError::DecodeFailed),
            Ok(read) => filled += read,
        }
    }
    Ok(true)
}

fn decode_pcm_samples(bytes: &[u8; PCM_FRAME_BYTES]) -> Result<[f32; FFT_SIZE], SpectrumError> {
    let mut samples = [0.0_f32; FFT_SIZE];
    let mut non_finite = 0;
    for (sample, encoded) in samples.iter_mut().zip(bytes.chunks_exact(size_of::<f32>())) {
        *sample = f32::from_le_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        if !sample.is_finite() {
            non_finite += 1;
            *sample = 0.0;
        }
    }
    if non_finite > MAX_NON_FINITE_SAMPLES {
        return Err(SpectrumError::DecodeFailed);
    }
    Ok(samples)
}

async fn kill_and_wait_spectrum(mut child: Box<dyn SpectrumProcess>) -> Result<(), SpectrumError> {
    let _ = child.start_kill();
    match tokio::time::timeout(CHILD_REAP_GRACE, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err(SpectrumError::ResourceLimit),
        Err(_) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            Err(SpectrumError::ResourceLimit)
        }
    }
}

type SpectrumProcessStdout = Pin<Box<dyn AsyncRead + Send + 'static>>;

#[async_trait]
trait SpectrumProcess: Send {
    fn take_stdout(&mut self) -> Option<SpectrumProcessStdout>;
    fn start_kill(&mut self) -> io::Result<()>;
    async fn wait(&mut self) -> io::Result<bool>;
}

trait SpectrumProcessLauncher: Send + Sync {
    fn spawn(&self, executable: &Path, args: &[OsString]) -> io::Result<Box<dyn SpectrumProcess>>;
}

struct TokioSpectrumProcessLauncher;

impl SpectrumProcessLauncher for TokioSpectrumProcessLauncher {
    fn spawn(&self, executable: &Path, args: &[OsString]) -> io::Result<Box<dyn SpectrumProcess>> {
        let child = Command::new(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Box::new(TokioSpectrumProcess { child }))
    }
}

struct TokioSpectrumProcess {
    child: tokio::process::Child,
}

#[async_trait]
impl SpectrumProcess for TokioSpectrumProcess {
    fn take_stdout(&mut self) -> Option<SpectrumProcessStdout> {
        self.child
            .stdout
            .take()
            .map(|stdout| Box::pin(stdout) as SpectrumProcessStdout)
    }

    fn start_kill(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }

    async fn wait(&mut self) -> io::Result<bool> {
        self.child.wait().await.map(|status| status.success())
    }
}

fn ffmpeg_arguments(request: &SpectrumRequest) -> Vec<OsString> {
    [
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-max_alloc".into(),
        MAX_FFMPEG_ALLOC_BYTES.to_string().into(),
        "-probesize".into(),
        MAX_FFMPEG_PROBE_BYTES.to_string().into(),
        "-analyzeduration".into(),
        MAX_FFMPEG_ANALYZE_MICROS.to_string().into(),
        "-rw_timeout".into(),
        FFMPEG_IO_TIMEOUT_MICROS.to_string().into(),
        "-readrate".into(),
        "1".into(),
        "-ss".into(),
        format!(
            "{}.{:03}",
            request.start_ms / 1_000,
            request.start_ms % 1_000
        )
        .into(),
        "-i".into(),
        request.stream_url.as_url().as_str().into(),
        "-vn".into(),
        "-t".into(),
        MAX_ANALYSIS_SECONDS.to_string().into(),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        ANALYSIS_SAMPLE_RATE.to_string().into(),
        "-acodec".into(),
        "pcm_f32le".into(),
        "-f".into(),
        "f32le".into(),
        "pipe:1".into(),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use crate::{app::Generation, domain::MediaId};
    use async_trait::async_trait;
    use tokio::io::{AsyncRead, ReadBuf};
    use tokio::sync::watch;
    use tokio_util::sync::CancellationToken;

    use super::{
        ANALYSIS_SAMPLE_RATE, FFT_SIZE, MAX_SPECTRUM_BANDS, MAX_SPECTRUM_LEVEL, SpectrumFrame,
        SpectrumFrameStore, SpectrumKey, SpectrumPresentation, SpectrumProcessor, SpectrumTarget,
    };

    fn sine_wave(frequency_hz: f32) -> Vec<f32> {
        let phase_step =
            2.0 * std::f32::consts::PI * frequency_hz / f32::from(ANALYSIS_SAMPLE_RATE);
        (0..FFT_SIZE)
            .scan(0.0_f32, |phase, _| {
                let sample = phase.sin();
                *phase += phase_step;
                Some(sample)
            })
            .collect()
    }

    fn strongest_band(frame: &SpectrumFrame) -> usize {
        frame
            .levels()
            .iter()
            .enumerate()
            .max_by_key(|(_, level)| *level)
            .map_or(0, |(index, _)| index)
    }

    fn target(bands: u16, rows: u8) -> SpectrumTarget {
        SpectrumTarget::new(bands, rows).unwrap_or_else(|| panic!("valid spectrum target"))
    }

    fn key(generation: u64, media: &str, target: SpectrumTarget) -> SpectrumKey {
        SpectrumKey::new(
            Generation::new(generation),
            MediaId {
                provider: "youtube".to_owned(),
                video_id: media.to_owned(),
            },
            target,
        )
    }

    fn frame(levels: &[u8]) -> Arc<SpectrumFrame> {
        Arc::new(
            SpectrumFrame::new(levels.to_vec().into_boxed_slice())
                .unwrap_or_else(|| panic!("valid spectrum frame")),
        )
    }

    #[test]
    fn targets_bound_band_and_row_counts() {
        for (bands, rows) in [(1, 1), (64, 3)] {
            let target = target(bands, rows);
            assert_eq!(target.bands(), bands);
            assert_eq!(target.rows(), rows);
        }

        for (bands, rows) in [(0, 1), (65, 1), (1, 0), (1, 4)] {
            assert!(SpectrumTarget::new(bands, rows).is_none());
        }
    }

    #[test]
    fn frames_bound_levels_and_band_count() {
        assert!(SpectrumFrame::new(Box::new([])).is_none());
        assert!(SpectrumFrame::new(vec![0; 65].into_boxed_slice()).is_none());
        assert!(SpectrumFrame::new(vec![0, 24].into_boxed_slice()).is_some());
        assert!(SpectrumFrame::new(vec![25].into_boxed_slice()).is_none());
    }

    #[test]
    fn frame_debug_reports_only_count_and_redacts_sentinel_levels() {
        let frame = SpectrumFrame::new(vec![17, 18, 19, 20].into_boxed_slice())
            .unwrap_or_else(|| panic!("bounded levels must form a frame"));

        let debug = format!("{frame:?}");

        assert!(debug.contains("level_count"));
        for level in ["17", "18", "19", "20"] {
            assert!(!debug.contains(level));
        }
    }

    #[test]
    fn publication_requires_exact_target_size() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(3, 2));
        let run = store
            .request(key)
            .unwrap_or_else(|| panic!("valid request must start a run"));

        assert!(!store.publish(&run, frame(&[1, 2])));
        assert!(!store.publish(&run, frame(&[1, 2, 3, 4])));
        assert!(store.publish(&run, frame(&[1, 2, 3])));
    }

    #[test]
    fn newest_frame_replaces_the_capacity_one_slot() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(2, 1));
        let run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        let first = frame(&[1, 2]);
        let newest = frame(&[3, 4]);

        assert!(store.publish(&run, Arc::clone(&first)));
        assert!(store.publish(&run, Arc::clone(&newest)));

        let shown = store.presentation(&key);
        assert!(Arc::ptr_eq(
            shown.frame().unwrap_or_else(|| panic!("frame missing")),
            &newest
        ));
    }

    #[test]
    fn generation_media_and_target_must_all_match() {
        let store = SpectrumFrameStore::new();
        let current = key(7, "video-a", target(2, 1));
        let lease = store
            .request_with_lease(current.clone())
            .unwrap_or_else(|| panic!("valid request must allocate a lease"));

        for stale in [
            key(6, "video-a", target(2, 1)),
            key(7, "video-b", target(2, 1)),
            key(7, "video-a", target(2, 2)),
        ] {
            assert!(!store.publish_with_lease(&stale, lease, frame(&[1, 2])));
            assert_eq!(store.presentation(&stale), SpectrumPresentation::quiet());
        }
        assert_eq!(store.presentation(&current), SpectrumPresentation::quiet());
    }

    #[test]
    fn pause_retains_frame_and_resume_allocates_a_fresh_lease() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(2, 1));
        let old_run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        let retained = frame(&[5, 6]);
        assert!(store.publish(&old_run, Arc::clone(&retained)));

        assert!(store.pause(&old_run));
        let paused = store.presentation(&key);
        assert!(paused.paused());
        assert!(Arc::ptr_eq(
            paused.frame().unwrap_or_else(|| panic!("frame retained")),
            &retained
        ));

        let fresh_run = store
            .resume(&old_run)
            .unwrap_or_else(|| panic!("valid resume must start a run"));
        assert_ne!(old_run.lease, fresh_run.lease);
        assert!(!store.presentation(&key).paused());
    }

    #[test]
    fn retiring_run_failure_after_pause_preserves_the_frozen_frame() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(2, 1));
        let retiring_run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        let retained = frame(&[5, 6]);
        assert!(store.publish(&retiring_run, Arc::clone(&retained)));
        assert!(store.pause(&retiring_run));

        assert!(!store.fail(&retiring_run));
        let presentation = store.presentation(&key);
        assert!(presentation.paused());
        assert!(!presentation.failed());
        assert!(Arc::ptr_eq(
            presentation
                .frame()
                .unwrap_or_else(|| panic!("paused frame retained")),
            &retained
        ));
    }

    #[test]
    fn public_mutations_reject_an_old_same_key_run_after_restart() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(2, 1));
        let old_run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        assert!(store.pause(&old_run));
        let current_run = store
            .resume(&old_run)
            .unwrap_or_else(|| panic!("valid resume must start a run"));

        assert!(!store.publish(&old_run, frame(&[1, 2])));
        assert!(!store.fail(&old_run));
        assert!(store.publish(&current_run, frame(&[3, 4])));
    }

    #[test]
    fn old_same_key_run_cannot_pause_its_replacement() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(2, 1));
        let old_run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        let current_run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("replacement must start a run"));

        assert!(!store.pause(&old_run));
        assert!(!store.presentation(&key).paused());
        assert!(store.publish(&current_run, frame(&[3, 4])));
    }

    #[test]
    fn old_same_key_run_cannot_resume_or_invalidate_its_replacement() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(2, 1));
        let old_run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        let current_run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("replacement must start a run"));
        assert!(store.pause(&current_run));

        assert!(store.resume(&old_run).is_none());
        assert!(store.presentation(&key).paused());

        let resumed_run = store
            .resume(&current_run)
            .unwrap_or_else(|| panic!("current paused run must resume"));
        assert!(!store.publish(&current_run, frame(&[1, 2])));
        assert!(store.publish(&resumed_run, frame(&[3, 4])));
    }

    #[test]
    fn failure_produces_a_quiet_fallback_state() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(2, 1));
        let run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        assert!(store.publish(&run, frame(&[7, 8])));

        assert!(store.fail(&run));

        let presentation = store.presentation(&key);
        assert!(presentation.frame().is_none());
        assert!(presentation.failed());
        assert!(!store.publish(&run, frame(&[9, 10])));
    }

    #[test]
    fn lease_exhaustion_clears_a_previously_active_presentation() {
        let store = SpectrumFrameStore::new();
        let old_key = key(1, "video-a", target(2, 1));
        let old_run = store
            .request(old_key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        assert!(store.publish(&old_run, frame(&[7, 8])));
        store.next_lease.store(u64::MAX, Ordering::SeqCst);

        assert!(store.request(key(2, "video-b", target(2, 1))).is_none());
        assert_eq!(store.presentation(&old_key), SpectrumPresentation::quiet());
        assert!(!store.publish(&old_run, frame(&[9, 10])));
    }

    #[test]
    fn redraw_revisions_are_nonblocking_and_coalesced() {
        let store = SpectrumFrameStore::new();
        let key = key(1, "video-a", target(1, 1));
        let run = store
            .request(key)
            .unwrap_or_else(|| panic!("valid request must start a run"));
        let mut redraw = store.subscribe_redraw();

        for level in 0..=24 {
            assert!(store.publish(&run, frame(&[level])));
        }

        assert!(redraw.has_changed().unwrap_or(false));
        assert_eq!(*redraw.borrow_and_update(), 25);
        assert!(!redraw.has_changed().unwrap_or(true));
    }

    #[test]
    fn clear_notifies_only_for_some_to_none_transition() {
        let store = SpectrumFrameStore::new();
        let mut redraw = store.subscribe_redraw();

        store.clear();
        assert!(!redraw.has_changed().unwrap_or(true));

        let key = key(1, "video-a", target(1, 1));
        let _run = store
            .request(key)
            .unwrap_or_else(|| panic!("valid request must start a run"));
        store.clear();
        assert!(redraw.has_changed().unwrap_or(false));
        assert_eq!(*redraw.borrow_and_update(), 1);

        store.clear();
        assert!(!redraw.has_changed().unwrap_or(true));
    }

    #[test]
    fn debug_exposes_counts_and_status_but_not_ids_or_levels() {
        let store = SpectrumFrameStore::new();
        let key = key(93, "secret-video-id", target(4, 2));
        let run = store
            .request(key.clone())
            .unwrap_or_else(|| panic!("valid request must start a run"));
        assert!(store.publish(&run, frame(&[17, 18, 19, 20])));

        let key_debug = format!("{key:?}");
        let run_debug = format!("{run:?}");
        let frame_debug = format!("{:?}", store.presentation(&key));
        let store_debug = format!("{store:?}");
        for debug in [&key_debug, &run_debug, &frame_debug, &store_debug] {
            assert!(!debug.contains("secret-video-id"));
            assert!(!debug.contains("youtube"));
            assert!(!debug.contains("17"));
            assert!(!debug.contains("18"));
            assert!(!debug.contains("19"));
            assert!(!debug.contains("20"));
        }
        assert!(store_debug.contains("band_count"));
        assert!(store_debug.contains("paused"));
        assert!(store_debug.contains("failed"));
    }

    #[test]
    fn fft_silence_produces_zero_levels() {
        let mut processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));

        let frame = processor
            .process(&vec![0.0; FFT_SIZE])
            .unwrap_or_else(|| panic!("an exact analysis window must produce a frame"));

        assert_eq!(frame.levels(), &[0; 16]);
    }

    #[test]
    fn fft_low_tone_emphasizes_bass_bands() {
        let mut processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));
        let frame = processor
            .process(&sine_wave(80.0))
            .unwrap_or_else(|| panic!("tone must produce a frame"));

        assert!(strongest_band(&frame) < 5);
    }

    #[test]
    fn fft_one_kilohertz_tone_emphasizes_middle_bands() {
        let mut processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));
        let frame = processor
            .process(&sine_wave(1_000.0))
            .unwrap_or_else(|| panic!("tone must produce a frame"));

        assert!((6..12).contains(&strongest_band(&frame)));
    }

    #[test]
    fn fft_three_kilohertz_tone_emphasizes_upper_bands() {
        let mut processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));
        let frame = processor
            .process(&sine_wave(3_000.0))
            .unwrap_or_else(|| panic!("tone must produce a frame"));

        assert!(strongest_band(&frame) >= 12);
    }

    #[test]
    fn fft_non_finite_samples_are_normalized_safely() {
        let mut samples = sine_wave(1_000.0);
        samples[1] = f32::NAN;
        samples[2] = f32::INFINITY;
        samples[3] = f32::NEG_INFINITY;
        let mut processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));

        let frame = processor
            .process(&samples)
            .unwrap_or_else(|| panic!("non-finite samples must be normalized"));

        assert!(
            frame
                .levels()
                .iter()
                .all(|level| *level <= MAX_SPECTRUM_LEVEL)
        );
    }

    #[test]
    fn fft_requires_exactly_one_analysis_window() {
        let mut processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));

        assert!(processor.process(&vec![0.0; FFT_SIZE - 1]).is_none());
        assert!(processor.process(&vec![0.0; FFT_SIZE + 1]).is_none());
        assert!(processor.process(&vec![0.0; FFT_SIZE]).is_some());
    }

    #[test]
    fn fft_output_is_bounded_for_every_supported_band_count() {
        for bands in 1..=MAX_SPECTRUM_BANDS {
            let mut processor = SpectrumProcessor::new(bands)
                .unwrap_or_else(|| panic!("supported band count must create a processor"));
            let frame = processor
                .process(&sine_wave(440.0))
                .unwrap_or_else(|| panic!("tone must produce a frame"));

            assert_eq!(frame.levels().len(), bands);
            assert!(
                frame
                    .levels()
                    .iter()
                    .all(|level| *level <= MAX_SPECTRUM_LEVEL)
            );
        }
    }

    #[test]
    fn fft_attack_is_fast_and_decay_is_slower_and_deterministic() {
        let tone = sine_wave(1_000.0);
        let silence = vec![0.0; FFT_SIZE];
        let mut first = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));
        let mut second = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));

        let attack = first
            .process(&tone)
            .unwrap_or_else(|| panic!("tone must produce a frame"));
        let first_decay = first
            .process(&silence)
            .unwrap_or_else(|| panic!("silence must produce a frame"));
        let second_decay = first
            .process(&silence)
            .unwrap_or_else(|| panic!("silence must produce a frame"));
        let repeated_attack = second
            .process(&tone)
            .unwrap_or_else(|| panic!("tone must produce a frame"));
        let repeated_decay = second
            .process(&silence)
            .unwrap_or_else(|| panic!("silence must produce a frame"));
        let peak = strongest_band(&attack);

        assert!(attack.levels()[peak] > first_decay.levels()[peak]);
        assert!(first_decay.levels()[peak] > second_decay.levels()[peak]);
        assert!(first_decay.levels()[peak] > 0);
        assert_eq!(attack.levels(), repeated_attack.levels());
        assert_eq!(first_decay.levels(), repeated_decay.levels());
    }

    #[test]
    fn fft_analyzer_debug_has_no_sample_or_rendering_payload() {
        let processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));

        let debug = format!("{processor:?}");

        assert!(debug.contains("SpectrumProcessor"));
        assert!(!debug.contains('█'));
        assert!(!debug.contains("levels"));
        assert!(!debug.contains("samples"));
    }

    #[test]
    fn fft_every_supported_band_has_at_least_one_frequency_bin() {
        for bands in 1..=MAX_SPECTRUM_BANDS {
            let processor = SpectrumProcessor::new(bands)
                .unwrap_or_else(|| panic!("supported band count must create a processor"));
            let mut occupancy = vec![0; bands];
            for band in processor.bin_bands.iter().copied() {
                occupancy[band] += 1;
            }

            assert!(
                occupancy.iter().all(|bin_count| *bin_count >= 1),
                "all {bands} bands must map at least one usable bin"
            );
        }
    }

    #[test]
    fn fft_constant_offset_is_removed_before_windowing() {
        let mut processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));

        let frame = processor
            .process(&vec![0.25; FFT_SIZE])
            .unwrap_or_else(|| panic!("constant input must produce a frame"));

        assert!(frame.levels().iter().all(|level| *level == 0));
    }

    #[test]
    fn fft_nyquist_bin_uses_unique_bin_amplitude_scaling() {
        let amplitude = 0.01;
        let ordinary = sine_wave(1_000.0)
            .into_iter()
            .map(|sample| sample * amplitude)
            .collect::<Vec<_>>();
        let nyquist = (0..FFT_SIZE)
            .map(|sample| {
                if sample % 2 == 0 {
                    amplitude
                } else {
                    -amplitude
                }
            })
            .collect::<Vec<_>>();
        let mut ordinary_processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));
        let mut nyquist_processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));

        let ordinary_peak = ordinary_processor
            .process(&ordinary)
            .and_then(|frame| frame.levels().iter().copied().max())
            .unwrap_or(0);
        let nyquist_peak = nyquist_processor
            .process(&nyquist)
            .and_then(|frame| frame.levels().iter().copied().max())
            .unwrap_or(0);

        assert_eq!(nyquist_peak, ordinary_peak);
    }

    #[test]
    fn fft_processor_preallocates_required_in_place_scratch() {
        let processor = SpectrumProcessor::new(16)
            .unwrap_or_else(|| panic!("valid band count must create a processor"));

        assert_eq!(
            processor.scratch.len(),
            processor.fft.get_inplace_scratch_len()
        );
    }

    fn analysis_url(value: &str) -> crate::resolver::AnalysisStreamUrl {
        crate::resolver::ResolvedStream::new(
            MediaId {
                provider: "youtube".to_owned(),
                video_id: "source".to_owned(),
            },
            url::Url::parse(value).unwrap_or_else(|error| panic!("valid URL: {error}")),
            time::OffsetDateTime::UNIX_EPOCH,
        )
        .analysis_stream_url()
        .unwrap_or_else(|| panic!("HTTPS URL must be analysis eligible"))
    }

    fn spectrum_request(url: &str) -> super::SpectrumRequest {
        super::SpectrumRequest::new(key(1, "secret-media", target(8, 1)), analysis_url(url))
    }

    #[test]
    fn ffmpeg_arguments_are_direct_bounded_mono_pcm_and_keep_url_opaque() {
        let secret = "https://audio.invalid/stream?token=secret&next=-vn";
        let request = spectrum_request(secret)
            .with_start_ms(12_345)
            .with_max_fps(15);
        let arguments = super::ffmpeg_arguments(&request);
        let strings = arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();

        assert_eq!(
            strings.iter().filter(|arg| arg.as_ref() == secret).count(),
            1
        );
        for pair in [
            ["-readrate", "1"],
            ["-ss", "12.345"],
            ["-ac", "1"],
            ["-ar", "8000"],
            ["-acodec", "pcm_f32le"],
            ["-f", "f32le"],
        ] {
            assert!(strings.windows(2).any(|window| window == pair));
        }
        assert!(strings.iter().any(|arg| arg.as_ref() == "-vn"));
        assert_eq!(strings.last().map(AsRef::<str>::as_ref), Some("pipe:1"));
        assert!(
            strings
                .iter()
                .all(|arg| arg.as_ref() != "sh" && arg.as_ref() != "-c")
        );
    }

    #[test]
    fn ffmpeg_request_decoder_and_error_debug_are_redacted() {
        let request = spectrum_request("https://audio.invalid/stream?token=do-not-log");
        let decoder = super::FfmpegSpectrumDecoder::new("/secret/path/ffmpeg");

        for debug in [
            format!("{request:?}"),
            format!("{decoder:?}"),
            format!("{:?}", super::SpectrumError::DecodeFailed),
            super::SpectrumError::DecodeFailed.to_string(),
        ] {
            assert!(!debug.contains("do-not-log"));
            assert!(!debug.contains("secret-media"));
            assert!(!debug.contains("youtube"));
            assert!(!debug.contains("secret/path"));
        }
    }

    #[test]
    fn ffmpeg_pcm_runtime_bound_is_independent_of_render_fps() {
        assert_eq!(
            super::max_pcm_frame_count(),
            16 * super::MAX_ANALYSIS_SECONDS
        );
    }

    fn pcm_bytes(samples: impl IntoIterator<Item = f32>) -> Vec<u8> {
        samples
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect::<Vec<_>>()
    }

    async fn wait_until(description: &str, mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_millis(500), async {
            while !condition() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
    }

    struct PendingReader;

    impl AsyncRead for PendingReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    struct ErrorReader;

    impl AsyncRead for ErrorReader {
        fn poll_read(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("sentinel raw decoder error")))
        }
    }

    enum FakeStdout {
        Bytes(Vec<u8>),
        Pending,
        Error,
    }

    struct FakeProcessLauncher {
        stdout: Mutex<Option<FakeStdout>>,
        block_wait_until_killed: bool,
        wait_gate: Option<Arc<tokio::sync::Notify>>,
        spawned: Option<Arc<AtomicUsize>>,
        killed: Arc<AtomicUsize>,
        waited: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    impl super::SpectrumProcessLauncher for FakeProcessLauncher {
        fn spawn(
            &self,
            _executable: &std::path::Path,
            _args: &[std::ffi::OsString],
        ) -> io::Result<Box<dyn super::SpectrumProcess>> {
            if let Some(spawned) = &self.spawned {
                spawned.fetch_add(1, Ordering::SeqCst);
            }
            let source = self
                .stdout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .unwrap_or(FakeStdout::Bytes(Vec::new()));
            let stdout: super::SpectrumProcessStdout = match source {
                FakeStdout::Bytes(bytes) => Box::pin(std::io::Cursor::new(bytes)),
                FakeStdout::Pending => Box::pin(PendingReader),
                FakeStdout::Error => Box::pin(ErrorReader),
            };
            Ok(Box::new(FakeProcess {
                stdout: Some(stdout),
                block_wait_until_killed: self.block_wait_until_killed,
                wait_gate: self.wait_gate.clone(),
                killed: Arc::clone(&self.killed),
                waited: Arc::clone(&self.waited),
                dropped: Arc::clone(&self.dropped),
            }))
        }
    }

    struct FakeProcess {
        stdout: Option<super::SpectrumProcessStdout>,
        block_wait_until_killed: bool,
        wait_gate: Option<Arc<tokio::sync::Notify>>,
        killed: Arc<AtomicUsize>,
        waited: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for FakeProcess {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl super::SpectrumProcess for FakeProcess {
        fn take_stdout(&mut self) -> Option<super::SpectrumProcessStdout> {
            self.stdout.take()
        }

        fn start_kill(&mut self) -> io::Result<()> {
            self.killed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn wait(&mut self) -> io::Result<bool> {
            self.waited.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.wait_gate {
                gate.notified().await;
                return Ok(false);
            }
            if self.block_wait_until_killed && self.killed.load(Ordering::SeqCst) == 0 {
                std::future::pending().await
            } else {
                Ok(true)
            }
        }
    }

    fn fake_decoder(
        stdout: FakeStdout,
    ) -> (
        super::FfmpegSpectrumDecoder,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let spawned = Arc::new(AtomicUsize::new(0));
        let killed = Arc::new(AtomicUsize::new(0));
        let waited = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let decoder = super::FfmpegSpectrumDecoder::new("ffmpeg").with_launcher(Arc::new(
            FakeProcessLauncher {
                stdout: Mutex::new(Some(stdout)),
                block_wait_until_killed: false,
                wait_gate: None,
                spawned: Some(Arc::clone(&spawned)),
                killed: Arc::clone(&killed),
                waited: Arc::clone(&waited),
                dropped: Arc::clone(&dropped),
            },
        ));
        (decoder, spawned, killed, waited, dropped)
    }

    #[tokio::test]
    async fn ffmpeg_exact_little_endian_window_publishes_one_frame() {
        let (decoder, _spawned, killed, waited, dropped) = fake_decoder(FakeStdout::Bytes(
            pcm_bytes(std::iter::repeat_n(0.0, super::FFT_SIZE)),
        ));
        let (output, frames) = watch::channel(None);

        assert_eq!(
            super::SpectrumDecoder::decode(
                &decoder,
                spectrum_request("https://audio.invalid/exact"),
                output,
                CancellationToken::new(),
            )
            .await,
            Ok(())
        );
        let frame = frames
            .borrow()
            .clone()
            .and_then(Result::ok)
            .unwrap_or_else(|| panic!("exact PCM frame must publish"));
        assert_eq!(frame.levels(), &[0; 8]);
        assert_eq!(killed.load(Ordering::SeqCst), 0);
        assert_eq!(waited.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ffmpeg_partial_oversized_nan_heavy_and_malformed_pcm_fail_statically() {
        let cases = [
            FakeStdout::Bytes(vec![0; super::PCM_FRAME_BYTES - 1]),
            FakeStdout::Bytes(vec![0; super::PCM_FRAME_BYTES + 1]),
            FakeStdout::Bytes(pcm_bytes(std::iter::repeat_n(f32::NAN, super::FFT_SIZE))),
            FakeStdout::Error,
        ];
        for (case, source) in cases.into_iter().enumerate() {
            let (decoder, _spawned, killed, _waited, _dropped) = fake_decoder(source);
            let (output, _frames) = watch::channel(None);
            let result = super::SpectrumDecoder::decode(
                &decoder,
                spectrum_request("https://audio.invalid/bad?secret=value"),
                output,
                CancellationToken::new(),
            )
            .await;
            let Err(error) = result else {
                panic!("invalid PCM must fail");
            };
            assert_eq!(error, super::SpectrumError::DecodeFailed);
            assert!(!error.to_string().contains("secret"));
            assert_eq!(killed.load(Ordering::SeqCst), 1, "case {case}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ffmpeg_wall_timeout_kills_and_reaps_stalled_stdout() {
        let (decoder, _spawned, killed, waited, _dropped) = fake_decoder(FakeStdout::Pending);
        let decoder = Arc::new(decoder.with_process_timeout(Duration::from_millis(5)));
        let (output, _frames) = watch::channel(None);
        let task = tokio::spawn(async move {
            super::SpectrumDecoder::decode(
                decoder.as_ref(),
                spectrum_request("https://audio.invalid/stalled"),
                output,
                CancellationToken::new(),
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(5)).await;

        assert_eq!(
            task.await.unwrap_or_else(|error| panic!("join: {error}")),
            Err(super::SpectrumError::ResourceLimit)
        );
        assert_eq!(killed.load(Ordering::SeqCst), 1);
        assert_eq!(waited.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ffmpeg_cancellation_kills_and_reaps_stalled_stdout() {
        let (decoder, spawned, killed, waited, _dropped) = fake_decoder(FakeStdout::Pending);
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (output, _frames) = watch::channel(None);
        let task = tokio::spawn(async move {
            super::SpectrumDecoder::decode(
                &decoder,
                spectrum_request("https://audio.invalid/cancel"),
                output,
                task_cancel,
            )
            .await
        });
        wait_until("FFmpeg launch before cancellation", || {
            spawned.load(Ordering::SeqCst) == 1
        })
        .await;
        cancel.cancel();

        assert_eq!(
            task.await.unwrap_or_else(|error| panic!("join: {error}")),
            Ok(())
        );
        assert_eq!(killed.load(Ordering::SeqCst), 1);
        assert_eq!(waited.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ffmpeg_pre_cancelled_request_does_not_spawn_or_touch_the_network() {
        let spawned = Arc::new(AtomicUsize::new(0));
        let killed = Arc::new(AtomicUsize::new(0));
        let waited = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let decoder = super::FfmpegSpectrumDecoder::new("ffmpeg").with_launcher(Arc::new(
            FakeProcessLauncher {
                stdout: Mutex::new(Some(FakeStdout::Pending)),
                block_wait_until_killed: false,
                wait_gate: None,
                spawned: Some(Arc::clone(&spawned)),
                killed: Arc::clone(&killed),
                waited: Arc::clone(&waited),
                dropped: Arc::clone(&dropped),
            },
        ));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (output, _frames) = watch::channel(None);

        assert_eq!(
            super::SpectrumDecoder::decode(
                &decoder,
                spectrum_request("https://audio.invalid/must-not-connect"),
                output,
                cancel,
            )
            .await,
            Ok(())
        );
        assert_eq!(spawned.load(Ordering::SeqCst), 0);
        assert_eq!(killed.load(Ordering::SeqCst), 0);
        assert_eq!(waited.load(Ordering::SeqCst), 0);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn ffmpeg_noncooperative_post_kill_wait_is_bounded_and_retains_reaper_ownership() {
        let killed = Arc::new(AtomicUsize::new(0));
        let waited = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let spawned = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let decoder = super::FfmpegSpectrumDecoder::new("ffmpeg").with_launcher(Arc::new(
            FakeProcessLauncher {
                stdout: Mutex::new(Some(FakeStdout::Pending)),
                block_wait_until_killed: false,
                wait_gate: Some(Arc::clone(&gate)),
                spawned: Some(Arc::clone(&spawned)),
                killed: Arc::clone(&killed),
                waited: Arc::clone(&waited),
                dropped: Arc::clone(&dropped),
            },
        ));
        let cancel = CancellationToken::new();
        let task_cancel = cancel.clone();
        let (output, _frames) = watch::channel(None);
        let task = tokio::spawn(async move {
            super::SpectrumDecoder::decode(
                &decoder,
                spectrum_request("https://audio.invalid/noncooperative"),
                output,
                task_cancel,
            )
            .await
        });
        wait_until("FFmpeg launch", || spawned.load(Ordering::SeqCst) == 1).await;
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_millis(500), task).await;

        assert_eq!(
            result
                .unwrap_or_else(|_| panic!("decoder cancellation exceeded bound"))
                .unwrap_or_else(|error| panic!("join: {error}")),
            Err(super::SpectrumError::ResourceLimit),
        );
        assert_eq!(killed.load(Ordering::SeqCst), 1);
        wait_until("post-kill child wait", || {
            waited.load(Ordering::SeqCst) >= 1
        })
        .await;
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        gate.notify_one();
        wait_until("detached child reaper", || {
            dropped.load(Ordering::SeqCst) == 1
        })
        .await;
    }

    #[derive(Default)]
    struct RecordingPacer {
        waits: Mutex<Vec<Duration>>,
    }

    #[async_trait]
    impl super::SpectrumPacer for RecordingPacer {
        async fn wait(&self, duration: Duration) {
            self.waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(duration);
            tokio::task::yield_now().await;
        }
    }

    struct BurstDecoder {
        frames: Vec<Arc<super::SpectrumFrame>>,
    }

    #[async_trait]
    impl super::SpectrumDecoder for BurstDecoder {
        async fn decode(
            &self,
            _request: super::SpectrumRequest,
            output: watch::Sender<Option<super::SpectrumFrameOutput>>,
            _cancel: CancellationToken,
        ) -> Result<(), super::SpectrumError> {
            for frame in &self.frames {
                output.send_replace(Some(Ok(Arc::clone(frame))));
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn worker_paces_and_capacity_one_output_keeps_newest_frame() {
        let store = Arc::new(super::SpectrumFrameStore::new());
        let pacer = Arc::new(RecordingPacer::default());
        let newest = frame(&[8; 8]);
        let mut worker = super::SpectrumWorker::spawn(
            Arc::new(BurstDecoder {
                frames: vec![frame(&[1; 8]), Arc::clone(&newest)],
            }),
            pacer.clone(),
            Arc::clone(&store),
            8,
        );
        let request = spectrum_request("https://audio.invalid/burst");
        let key = request.key().clone();

        worker.replace(request);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        assert!(Arc::ptr_eq(
            store
                .presentation(&key)
                .frame()
                .unwrap_or_else(|| panic!("frame")),
            &newest
        ));
        assert!(
            pacer
                .waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .all(|wait| *wait == Duration::from_millis(125))
        );
        worker.shutdown().await;
    }

    #[tokio::test]
    async fn worker_preserves_the_configured_thirty_fps_interval() {
        let store = Arc::new(super::SpectrumFrameStore::new());
        let pacer = Arc::new(RecordingPacer::default());
        let mut worker = super::SpectrumWorker::spawn(
            Arc::new(BurstDecoder {
                frames: vec![frame(&[8; 8])],
            }),
            pacer.clone(),
            Arc::clone(&store),
            30,
        );

        worker.replace(spectrum_request("https://audio.invalid/thirty-fps"));
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            pacer
                .waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .first()
                .copied(),
            Some(Duration::from_secs_f64(1.0 / 30.0))
        );
        worker.shutdown().await;
    }

    struct ControlledDecoder {
        frames: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Arc<super::SpectrumFrame>>>,
    }

    #[async_trait]
    impl super::SpectrumDecoder for ControlledDecoder {
        async fn decode(
            &self,
            _request: super::SpectrumRequest,
            output: watch::Sender<Option<super::SpectrumFrameOutput>>,
            cancel: CancellationToken,
        ) -> Result<(), super::SpectrumError> {
            let mut frames = self.frames.lock().await;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ok(()),
                    frame = frames.recv() => match frame {
                        Some(frame) => { output.send_replace(Some(Ok(frame))); }
                        None => return Ok(()),
                    }
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn worker_max_fps_uses_fake_time_and_slow_publisher_takes_newest_output() {
        let store = Arc::new(super::SpectrumFrameStore::new());
        let (frames, frame_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut worker = super::SpectrumWorker::spawn(
            Arc::new(ControlledDecoder {
                frames: tokio::sync::Mutex::new(frame_rx),
            }),
            Arc::new(super::TokioSpectrumPacer),
            Arc::clone(&store),
            10,
        );
        let request = spectrum_request("https://audio.invalid/paced");
        let key = request.key().clone();
        worker.replace(request);
        frames
            .send(frame(&[1; 8]))
            .unwrap_or_else(|_| panic!("decoder open"));
        wait_until("first paced spectrum frame", || {
            store.presentation(&key).frame().is_some()
        })
        .await;
        frames
            .send(frame(&[2; 8]))
            .unwrap_or_else(|_| panic!("decoder open"));
        frames
            .send(frame(&[3; 8]))
            .unwrap_or_else(|_| panic!("decoder open"));
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            store
                .presentation(&key)
                .frame()
                .map(|frame| frame.levels()[0]),
            Some(1)
        );

        tokio::time::advance(Duration::from_millis(100)).await;
        wait_until("newest paced spectrum frame", || {
            store
                .presentation(&key)
                .frame()
                .is_some_and(|frame| frame.levels()[0] == 3)
        })
        .await;
        assert_eq!(
            store
                .presentation(&key)
                .frame()
                .map(|frame| frame.levels()[0]),
            Some(3)
        );
        worker.shutdown().await;
    }

    struct RequestDecoder {
        starts: tokio::sync::mpsc::UnboundedSender<u64>,
        reaped: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl super::SpectrumDecoder for RequestDecoder {
        async fn decode(
            &self,
            request: super::SpectrumRequest,
            _output: watch::Sender<Option<super::SpectrumFrameOutput>>,
            cancel: CancellationToken,
        ) -> Result<(), super::SpectrumError> {
            let _ = self.starts.send(request.start_ms);
            cancel.cancelled().await;
            self.reaped.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn worker_pause_resume_seek_replace_clear_and_shutdown_retire_old_work() {
        let (starts, mut start_rx) = tokio::sync::mpsc::unbounded_channel();
        let reaped = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(super::SpectrumFrameStore::new());
        let mut worker = super::SpectrumWorker::spawn(
            Arc::new(RequestDecoder {
                starts,
                reaped: Arc::clone(&reaped),
            }),
            Arc::new(RecordingPacer::default()),
            Arc::clone(&store),
            15,
        );
        let request = spectrum_request("https://audio.invalid/lifecycle");
        let key = request.key().clone();
        worker.replace(request);
        assert_eq!(start_rx.recv().await, Some(0));
        let active_run = worker
            .active
            .as_ref()
            .map_or_else(|| panic!("active run"), |active| active.run.clone());
        assert!(store.publish(&active_run, frame(&[3; 8])));

        worker.pause();
        wait_until("paused decoder cleanup", || {
            reaped.load(Ordering::SeqCst) >= 1
        })
        .await;
        assert!(store.presentation(&key).paused());
        assert!(store.presentation(&key).frame().is_some());
        worker.resume(9_876);
        assert_eq!(start_rx.recv().await, Some(9_876));
        assert_ne!(
            active_run.lease,
            worker
                .active
                .as_ref()
                .map_or(active_run.lease, |active| active.run.lease)
        );

        worker.seek(12_000);
        assert_eq!(start_rx.recv().await, Some(12_000));
        worker.replace(spectrum_request("https://audio.invalid/replacement"));
        assert_eq!(start_rx.recv().await, Some(0));
        worker.clear();
        assert_eq!(
            store.presentation(&key),
            super::SpectrumPresentation::quiet()
        );
        worker.replace(spectrum_request("https://audio.invalid/shutdown"));
        assert_eq!(start_rx.recv().await, Some(0));
        worker.shutdown().await;
        assert_eq!(reaped.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn worker_seek_while_paused_keeps_frozen_run_and_defers_decode_until_resume() {
        let (starts, mut start_rx) = tokio::sync::mpsc::unbounded_channel();
        let reaped = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(super::SpectrumFrameStore::new());
        let mut worker = super::SpectrumWorker::spawn(
            Arc::new(RequestDecoder {
                starts,
                reaped: Arc::clone(&reaped),
            }),
            Arc::new(RecordingPacer::default()),
            Arc::clone(&store),
            15,
        );
        let request = spectrum_request("https://audio.invalid/paused-seek");
        let key = request.key().clone();
        worker.replace(request);
        assert_eq!(start_rx.recv().await, Some(0));
        let original_run = worker
            .active
            .as_ref()
            .map_or_else(|| panic!("active run"), |active| active.run.clone());
        let frozen = frame(&[6; 8]);
        assert!(store.publish(&original_run, Arc::clone(&frozen)));
        worker.pause();
        wait_until("paused decoder cleanup", || {
            reaped.load(Ordering::SeqCst) == 1
        })
        .await;

        worker.seek(44_321);

        assert!(matches!(
            start_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        let presentation = store.presentation(&key);
        assert!(presentation.paused());
        assert!(Arc::ptr_eq(
            presentation
                .frame()
                .unwrap_or_else(|| panic!("frozen frame retained")),
            &frozen
        ));
        assert_eq!(
            worker
                .paused
                .as_ref()
                .map(|(request, run)| (request.start_ms, run.lease)),
            Some((44_321, original_run.lease))
        );

        worker.resume(44_321);
        assert_eq!(start_rx.recv().await, Some(44_321));
        assert_ne!(
            worker.active.as_ref().map(|active| active.run.lease),
            Some(original_run.lease)
        );
        assert!(matches!(
            start_rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        worker.shutdown().await;
    }

    struct NonCooperativeDecoder {
        started: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    struct DecodeDropGuard(Arc<AtomicUsize>);

    impl Drop for DecodeDropGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl super::SpectrumDecoder for NonCooperativeDecoder {
        async fn decode(
            &self,
            _request: super::SpectrumRequest,
            _output: watch::Sender<Option<super::SpectrumFrameOutput>>,
            _cancel: CancellationToken,
        ) -> Result<(), super::SpectrumError> {
            let _guard = DecodeDropGuard(Arc::clone(&self.dropped));
            self.started.fetch_add(1, Ordering::SeqCst);
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn worker_replacement_and_shutdown_bound_noncooperative_decoder_cleanup() {
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut worker = super::SpectrumWorker::spawn(
            Arc::new(NonCooperativeDecoder {
                started: Arc::clone(&started),
                dropped: Arc::clone(&dropped),
            }),
            Arc::new(RecordingPacer::default()),
            Arc::new(super::SpectrumFrameStore::new()),
            15,
        );
        worker.replace(spectrum_request("https://audio.invalid/first"));
        wait_until("first noncooperative decoder start", || {
            started.load(Ordering::SeqCst) >= 1
        })
        .await;
        worker.replace(spectrum_request("https://audio.invalid/second"));
        wait_until("second noncooperative decoder start", || {
            started.load(Ordering::SeqCst) >= 2
        })
        .await;

        tokio::time::timeout(Duration::from_millis(500), worker.shutdown())
            .await
            .unwrap_or_else(|_| panic!("shutdown did not bound decoder cleanup"));
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }
}
