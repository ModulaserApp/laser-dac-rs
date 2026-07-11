//! AVB DAC streaming backend implementation.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::{BufferEstimator, QueueDepthSource, RuntimeAuthorityEstimator};
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;
use crate::protocols::avb::{is_blacklisted_device, normalize_device_name};
use crate::resample::{CatmullInterp, StreamingResampler};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat};
use crossbeam_queue::ArrayQueue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const MIN_CHANNELS: u16 = 5;
const CHANNELS_XYRGB: usize = 5;
const CHANNELS_XYRGBI: usize = 6;
/// Duration of the queue buffer in milliseconds.
/// Chosen as a conservative jitter cushion while keeping queueing latency bounded.
const QUEUE_DURATION_MS: u32 = 200;
/// How long `connect` waits for the audio worker to report stream startup.
const INIT_TIMEOUT: Duration = Duration::from_secs(5);

fn queue_capacity_for_rate(sample_rate: u32) -> usize {
    (sample_rate as usize * QUEUE_DURATION_MS as usize) / 1000
}

/// Returns the cpal audio host to use for AVB output.
///
/// - Windows with the `asio` default feature: the ASIO host (recommended
///   for reliable multichannel output on pro audio interfaces).
/// - Otherwise: the cpal default host — CoreAudio on macOS, WASAPI on
///   Windows (when `asio` is disabled), ALSA on Linux.
fn get_audio_host() -> Result<cpal::Host> {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        cpal::host_from_id(cpal::HostId::Asio).map_err(|e| {
            Error::invalid_config(format!(
                "ASIO host not available (is the ASIO SDK installed?): {}",
                e
            ))
        })
    }
    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        Ok(cpal::default_host())
    }
}

#[derive(Debug, Clone)]
pub struct AvbSelector {
    pub name: String,
    pub duplicate_index: u16,
}

#[derive(Debug, Clone, Copy)]
struct OutputConfigRange {
    channels: u16,
    min_sample_rate: u32,
    max_sample_rate: u32,
    sample_format: SampleFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectedStreamConfig {
    channels: u16,
    sample_rate: u32,
    sample_format: SampleFormat,
}

struct DeviceRecord<D> {
    name: String,
    device: D,
    output_config_ranges: Vec<OutputConfigRange>,
    default_output_channels: Option<u16>,
    default_output_sample_rate: Option<u32>,
}

struct DeviceCandidate<D> {
    selector: AvbSelector,
    device: D,
    output_config_ranges: Vec<OutputConfigRange>,
    default_output_sample_rate: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
struct StreamPoint {
    x: f32,
    y: f32,
    r: f32,
    g: f32,
    b: f32,
    i: f32,
}

impl CatmullInterp for StreamPoint {
    fn catmull(s0: Self, s1: Self, s2: Self, s3: Self, t: f32) -> Self {
        use crate::resample::catmull_rom;
        // Catmull-Rom is non-monotone: it can overshoot the control points, so
        // an interpolated value may leave the input range even though every
        // source point is in range. Clamp the six fields to their valid domains
        // (XY ∈ [-1, 1], colors/intensity ∈ [0, 1]) before they reach the
        // galvo/premultiply path — an undershoot below 0 on both a color and
        // `i` would otherwise multiply into a spurious positive in the 5-channel
        // premultiply.
        StreamPoint {
            x: catmull_rom(s0.x, s1.x, s2.x, s3.x, t).clamp(-1.0, 1.0),
            y: catmull_rom(s0.y, s1.y, s2.y, s3.y, t).clamp(-1.0, 1.0),
            r: catmull_rom(s0.r, s1.r, s2.r, s3.r, t).clamp(0.0, 1.0),
            g: catmull_rom(s0.g, s1.g, s2.g, s3.g, t).clamp(0.0, 1.0),
            b: catmull_rom(s0.b, s1.b, s2.b, s3.b, t).clamp(0.0, 1.0),
            i: catmull_rom(s0.i, s1.i, s2.i, s3.i, t).clamp(0.0, 1.0),
        }
    }
}

struct RuntimeState {
    queue: ArrayQueue<StreamPoint>,
    sample_rate: u32,
    shutter_open: AtomicBool,
    last_x_bits: AtomicU32,
    last_y_bits: AtomicU32,
    /// Set by the cpal error callback when the stream dies (e.g. the device
    /// was unplugged). Producer-side calls observe it and surface a
    /// disconnected-class error so the driver's reconnect path engages.
    stream_failed: AtomicBool,
}

impl RuntimeState {
    fn new(shutter_open: bool, sample_rate: u32) -> Self {
        Self {
            queue: ArrayQueue::new(queue_capacity_for_rate(sample_rate)),
            sample_rate,
            shutter_open: AtomicBool::new(shutter_open),
            last_x_bits: AtomicU32::new(0.0f32.to_bits()),
            last_y_bits: AtomicU32::new(0.0f32.to_bits()),
            stream_failed: AtomicBool::new(false),
        }
    }

    fn clear_queue(&self) {
        while self.queue.pop().is_some() {}
    }

    fn queued_points(&self) -> u64 {
        self.queue.len() as u64
    }

    fn mark_stream_failed(&self) {
        self.stream_failed.store(true, Ordering::Release);
    }

    fn stream_failed(&self) -> bool {
        self.stream_failed.load(Ordering::Acquire)
    }
}

impl QueueDepthSource for RuntimeState {
    fn queued_points(&self) -> u64 {
        RuntimeState::queued_points(self)
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl RuntimeState {
    fn remaining_capacity(&self) -> usize {
        self.queue.capacity().saturating_sub(self.queue.len())
    }

    fn pop_point(&self) -> Option<StreamPoint> {
        self.queue.pop()
    }

    fn push_point(&self, point: StreamPoint) {
        self.queue
            .push(point)
            .expect("queue capacity validated before push");
    }

    fn has_capacity_for(&self, count: usize) -> bool {
        if count == 0 {
            return true;
        }
        self.remaining_capacity() >= count
    }

    fn set_last_xy(&self, x: f32, y: f32) {
        self.last_x_bits.store(x.to_bits(), Ordering::Release);
        self.last_y_bits.store(y.to_bits(), Ordering::Release);
    }

    fn last_xy(&self) -> (f32, f32) {
        (
            f32::from_bits(self.last_x_bits.load(Ordering::Acquire)),
            f32::from_bits(self.last_y_bits.load(Ordering::Acquire)),
        )
    }
}

trait RunningAudioStream {}

/// Result of resolving a selector to a concrete stream config, plus the count
/// of same-named devices visible at resolve time (for the reconnect identity
/// guard). `same_name_count` is `None` when the engine can't enumerate a stable
/// device set (test fakes), in which case the guard is skipped.
struct ResolvedConfig {
    config: SelectedStreamConfig,
    same_name_count: Option<usize>,
}

trait AudioEngine: Send + Sync {
    fn resolve_stream_config(&self, selector: &AvbSelector) -> Result<ResolvedConfig>;
    fn open_stream(
        &self,
        selector: &AvbSelector,
        stream_config: SelectedStreamConfig,
        runtime: Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>>;
}

struct CpalRunningStream {
    _stream: cpal::Stream,
}

impl RunningAudioStream for CpalRunningStream {}

struct CpalAudioEngine;

impl AudioEngine for CpalAudioEngine {
    fn resolve_stream_config(&self, selector: &AvbSelector) -> Result<ResolvedConfig> {
        // Enumerate once: derive both the stream config and the same-named
        // device count from a single candidate list (the worker thread does a
        // second, unavoidable enumeration to build the !Send stream).
        let candidates = collect_candidates()?;
        let key = normalize_device_name(&selector.name);
        let same_name_count = candidates
            .iter()
            .filter(|c| normalize_device_name(&c.selector.name) == key)
            .count();
        let candidate = candidates
            .into_iter()
            .find(|candidate| {
                candidate.selector.name == selector.name
                    && candidate.selector.duplicate_index == selector.duplicate_index
            })
            .ok_or_else(|| {
                Error::disconnected(
                    super::error::Error::DeviceNotFound(format!(
                        "{} (index {})",
                        selector.name, selector.duplicate_index
                    ))
                    .to_string(),
                )
            })?;
        Ok(ResolvedConfig {
            config: select_stream_config(&candidate)?,
            same_name_count: Some(same_name_count),
        })
    }

    fn open_stream(
        &self,
        selector: &AvbSelector,
        stream_config: SelectedStreamConfig,
        runtime: Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>> {
        let selected = select_device(selector)?;
        let output_channels = stream_config.channels as usize;
        let sample_format = stream_config.sample_format;

        let config = build_cpal_stream_config(stream_config);
        let stream = build_output_stream_for_format(
            &selected.device,
            &config,
            output_channels,
            sample_format,
            &runtime,
        )?;

        stream.play().map_err(Error::backend)?;

        Ok(Box::new(CpalRunningStream { _stream: stream }))
    }
}

/// Build an output stream for the given sample format, converting f32 samples
/// to the device's native format inside the callback.
fn build_output_stream_for_format(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    output_channels: usize,
    sample_format: SampleFormat,
    runtime: &Arc<RuntimeState>,
) -> Result<cpal::Stream> {
    let callback_state = Arc::clone(runtime);
    let err_state = Arc::clone(runtime);
    let err_fn = move |err: cpal::StreamError| {
        log::error!("AVB output stream error: {}", err);
        if matches!(err, cpal::StreamError::DeviceNotAvailable) {
            err_state.mark_stream_failed();
        }
    };

    let built = match sample_format {
        SampleFormat::F32 => device.build_output_stream(
            config,
            move |data: &mut [f32], _| fill_output_buffer(data, output_channels, &callback_state),
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_output_stream(
            config,
            move |data: &mut [i16], _| {
                fill_output_buffer_converted(data, output_channels, &callback_state)
            },
            err_fn,
            None,
        ),
        SampleFormat::I32 => device.build_output_stream(
            config,
            move |data: &mut [i32], _| {
                fill_output_buffer_converted(data, output_channels, &callback_state)
            },
            err_fn,
            None,
        ),
        _ => return Err(Error::backend(super::error::Error::UnsupportedOutputConfig)),
    };

    built.map_err(Error::backend)
}

/// AVB DAC backend using system audio output.
pub struct AvbBackend {
    selector: AvbSelector,
    runtime: Option<Arc<RuntimeState>>,
    stop_tx: Option<Sender<()>>,
    worker_handle: Option<JoinHandle<()>>,
    engine: Arc<dyn AudioEngine>,
    caps: DacCapabilities,
    desired_shutter_open: bool,
    init_timeout: Duration,
    /// Runtime-authoritative buffer estimator. Source is set on connect and
    /// cleared on disconnect; reports zero in between.
    estimator: RuntimeAuthorityEstimator,
    /// Stateful PPS→sample-rate resampler. Carries fractional phase and
    /// trailing input across `try_write_points` calls so interpolation spans
    /// chunk boundaries. Reset on connect/stop/disconnect; re-phased on PPS
    /// change. Rates are (from=pps, to=sample_rate); `1` is a placeholder until
    /// the first write establishes the real rates.
    resampler: StreamingResampler<StreamPoint>,
    /// Reusable scratch buffer for `StreamPoint` conversion of the input chunk,
    /// so the realtime `try_write_points` path doesn't allocate per call.
    resample_scratch: Vec<StreamPoint>,
    /// Number of same-named devices seen at scan time, used to detect device
    /// set changes that would make `duplicate_index` bind a different unit.
    scan_duplicate_count: Option<usize>,
    /// Whether we've already warned that the requested PPS exceeds the device
    /// sample rate (decimation). Reset on connect so each session warns once.
    warned_pps_exceeds_rate: bool,
}

impl AvbBackend {
    fn build(selector: AvbSelector, engine: Arc<dyn AudioEngine>) -> Self {
        Self {
            selector,
            runtime: None,
            stop_tx: None,
            worker_handle: None,
            engine,
            caps: super::default_capabilities(),
            desired_shutter_open: false,
            init_timeout: INIT_TIMEOUT,
            estimator: RuntimeAuthorityEstimator::new(),
            resampler: StreamingResampler::new(1, 1),
            resample_scratch: Vec::new(),
            scan_duplicate_count: None,
            warned_pps_exceeds_rate: false,
        }
    }

    /// Reset the "PPS exceeds sample rate" warning latch after a (re)connect so
    /// the next session logs the decimation notice once.
    ///
    /// We deliberately do *not* pin `caps.pps_max` to the device rate. PPS above
    /// the rate is handled by decimation in the write path (see `write`), which
    /// is the intended behavior; a hard cap would only be observable if it were
    /// wired into reconnect validation, and there it would *reject* such a
    /// session outright instead of decimating it — a regression, not a fix.
    fn reset_pps_warning(&mut self) {
        self.warned_pps_exceeds_rate = false;
    }

    pub fn new(name: String, duplicate_index: u16) -> Self {
        Self::build(
            AvbSelector {
                name,
                duplicate_index,
            },
            Arc::new(CpalAudioEngine),
        )
    }

    /// Build from a discovered selector, recording how many same-named devices
    /// were visible at scan time for the reconnect identity guard.
    pub(crate) fn from_selector_with_scan_count(
        selector: AvbSelector,
        scan_duplicate_count: usize,
    ) -> Self {
        let mut backend = Self::build(selector, Arc::new(CpalAudioEngine));
        backend.scan_duplicate_count = Some(scan_duplicate_count);
        backend
    }

    #[cfg(test)]
    fn with_engine_for_test(
        name: String,
        duplicate_index: u16,
        engine: Arc<dyn AudioEngine>,
    ) -> Self {
        Self::build(
            AvbSelector {
                name,
                duplicate_index,
            },
            engine,
        )
    }
}

impl DacBackend for AvbBackend {
    fn dac_type(&self) -> DacType {
        DacType::Avb
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        if self.is_connected() {
            log::debug!("AVB: connect called but already connected");
            return Ok(());
        }

        log::debug!(
            "AVB: connecting to {:?} (duplicate_index={})",
            self.selector.name,
            self.selector.duplicate_index
        );

        let resolved = self.engine.resolve_stream_config(&self.selector)?;

        // Reconnect identity guard: `duplicate_index` is a positional index
        // among same-named devices, so if the number of devices sharing this
        // name changed since scan time the index may now bind a *different*
        // physical unit. Refuse rather than silently drive the wrong laser.
        if let (Some(recorded), Some(current)) =
            (self.scan_duplicate_count, resolved.same_name_count)
        {
            if recorded != current {
                log::error!(
                    "AVB: device set for {:?} changed since scan ({} → {} same-named devices); \
                     refusing to bind by positional index",
                    self.selector.name,
                    recorded,
                    current
                );
                return Err(Error::disconnected(
                    super::error::Error::DeviceNotFound(format!(
                        "{}: same-named device count changed ({} → {}); rescan required",
                        self.selector.name, recorded, current
                    ))
                    .to_string(),
                ));
            }
        }

        let stream_config = resolved.config;
        log::info!(
            "AVB: selected {} channels at {}Hz ({:?})",
            stream_config.channels,
            stream_config.sample_rate,
            stream_config.sample_format,
        );

        // A fresh stream starts a fresh resample phase.
        self.resampler.reset();

        let runtime = Arc::new(RuntimeState::new(
            self.desired_shutter_open,
            stream_config.sample_rate,
        ));
        let runtime_for_worker = Arc::clone(&runtime);
        let selector = self.selector.clone();
        let engine = Arc::clone(&self.engine);

        let (stop_tx, stop_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            run_audio_worker(
                engine,
                selector,
                stream_config,
                runtime_for_worker,
                stop_rx,
                init_tx,
            );
        });

        match init_rx.recv_timeout(self.init_timeout) {
            Ok(Ok(())) => {
                log::info!(
                    "AVB: connected to {:?} (duplicate_index={}) at {}Hz",
                    self.selector.name,
                    self.selector.duplicate_index,
                    stream_config.sample_rate
                );
                self.reset_pps_warning();
                self.estimator
                    .set_source(Arc::clone(&runtime) as Arc<dyn QueueDepthSource>);
                self.runtime = Some(runtime);
                self.stop_tx = Some(stop_tx);
                self.worker_handle = Some(handle);
                Ok(())
            }
            Ok(Err(err)) => {
                log::error!("AVB: audio worker failed to start: {}", err);
                let _ = handle.join();
                Err(err)
            }
            Err(RecvTimeoutError::Disconnected) => {
                // The worker died (most likely panicked inside the audio
                // engine) before reporting stream startup; it has already
                // finished, so joining is safe and immediate.
                log::error!("AVB: audio worker exited before reporting stream startup");
                let _ = handle.join();
                Err(Error::backend(super::error::Error::StreamStartFailed))
            }
            Err(RecvTimeoutError::Timeout) => {
                // The worker is most likely blocked inside a driver call
                // (opening an audio stream can hang in misbehaving drivers).
                // Joining it here would block indefinitely, so detach
                // instead: dropping `stop_tx` disconnects the worker's stop
                // channel, making it shut down on its own if it ever
                // unblocks. Until then the thread (and its queue) stays
                // alive, so repeated connect attempts against a permanently
                // hung driver accumulate detached threads.
                log::error!("AVB: audio worker init timed out ({:?})", self.init_timeout);
                log::warn!(
                    "AVB: detaching audio worker for {:?}; it will clean itself up if the driver call ever returns",
                    self.selector.name
                );
                drop(handle);
                Err(Error::backend(super::error::Error::StreamStartFailed))
            }
        }
    }

    fn disconnect(&mut self) -> Result<()> {
        log::debug!(
            "AVB: disconnecting from {:?} (duplicate_index={})",
            self.selector.name,
            self.selector.duplicate_index
        );
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }

        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }

        if let Some(runtime) = self.runtime.take() {
            runtime.clear_queue();
        }
        self.estimator.clear_source();
        self.resampler.reset();

        log::info!("AVB: disconnected from {:?}", self.selector.name);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.runtime.is_some()
            && self.worker_handle.is_some()
            && self.runtime.as_ref().is_some_and(|rt| !rt.stream_failed())
    }

    fn stop(&mut self) -> Result<()> {
        self.desired_shutter_open = false;
        if let Some(runtime) = &self.runtime {
            runtime.shutter_open.store(false, Ordering::Release);
            runtime.clear_queue();
        }
        // The queue was just cleared; drop the carried resample phase too so
        // output resumes cleanly rather than continuing a stale trajectory.
        self.resampler.reset();
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        self.desired_shutter_open = open;
        if let Some(runtime) = &self.runtime {
            runtime.shutter_open.store(open, Ordering::Release);
        }
        Ok(())
    }
}

impl FifoBackend for AvbBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        // Surface a dead cpal stream (e.g. device unplugged) as a
        // disconnected-class error so the driver's reconnect path engages.
        if runtime.stream_failed() {
            return Err(Error::disconnected("AVB output stream failed"));
        }

        if points.is_empty() {
            return Ok(WriteOutcome::Written);
        }

        if pps > runtime.sample_rate && !self.warned_pps_exceeds_rate {
            log::warn!(
                "AVB: requested PPS {} exceeds device sample rate {}Hz — output will be decimated",
                pps,
                runtime.sample_rate
            );
            self.warned_pps_exceeds_rate = true;
        }

        // Keep the resampler phased to the current PPS (a PPS change re-phases).
        self.resampler.set_rates(pps.max(1), runtime.sample_rate);

        // Reserve queue capacity for exactly what the resampler will emit for
        // this chunk, given its carried phase; bail out cleanly if it won't fit.
        let output_len = self.resampler.pending_output_count(points.len());
        if !runtime.has_capacity_for(output_len) {
            return Ok(WriteOutcome::WouldBlock);
        }

        self.resample_scratch.clear();
        self.resample_scratch
            .extend(points.iter().map(StreamPoint::from));
        let scratch = std::mem::take(&mut self.resample_scratch);
        self.resampler.process(&scratch, |p| runtime.push_point(p));
        self.resample_scratch = scratch;

        Ok(WriteOutcome::Written)
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

pub fn discover_device_selectors() -> Result<Vec<AvbSelector>> {
    let candidates = collect_candidates()?;
    let selectors: Vec<AvbSelector> = candidates.into_iter().map(|c| c.selector).collect();
    if selectors.is_empty() {
        log::debug!("AVB: no candidate devices found");
    } else {
        for s in &selectors {
            log::debug!(
                "AVB: candidate device {:?} (duplicate_index={})",
                s.name,
                s.duplicate_index
            );
        }
    }
    Ok(selectors)
}

fn run_audio_worker(
    engine: Arc<dyn AudioEngine>,
    selector: AvbSelector,
    stream_config: SelectedStreamConfig,
    runtime: Arc<RuntimeState>,
    stop_rx: mpsc::Receiver<()>,
    init_tx: mpsc::Sender<Result<()>>,
) {
    log::debug!(
        "AVB: audio worker starting for {:?} (duplicate_index={})",
        selector.name,
        selector.duplicate_index
    );
    let stream = match engine.open_stream(&selector, stream_config, Arc::clone(&runtime)) {
        Ok(stream) => {
            log::debug!("AVB: audio stream opened successfully");
            stream
        }
        Err(err) => {
            log::error!("AVB: failed to open audio stream: {}", err);
            let _ = init_tx.send(Err(err));
            return;
        }
    };

    if init_tx.send(Ok(())).is_err() {
        // `connect` gave up waiting (init timeout) and detached this worker;
        // the disconnected stop channel makes the loop below exit right away.
        log::debug!("AVB: stream opened after connect gave up; shutting down");
    }

    loop {
        match stop_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(_) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    log::debug!("AVB: audio worker stopping, clearing queue");
    runtime.clear_queue();
    runtime.shutter_open.store(false, Ordering::Release);
    drop(stream);
    log::debug!("AVB: audio worker stopped");
}

fn select_device(selector: &AvbSelector) -> Result<DeviceCandidate<cpal::Device>> {
    log::debug!(
        "AVB: selecting device {:?} (duplicate_index={})",
        selector.name,
        selector.duplicate_index
    );
    let candidates = collect_candidates()?;
    log::debug!(
        "AVB: {} candidate(s) available for selection",
        candidates.len()
    );
    candidates
        .into_iter()
        .find(|candidate| {
            candidate.selector.name == selector.name
                && candidate.selector.duplicate_index == selector.duplicate_index
        })
        .ok_or_else(|| {
            log::warn!(
                "AVB: device {:?} (index {}) not found among candidates",
                selector.name,
                selector.duplicate_index
            );
            Error::disconnected(
                super::error::Error::DeviceNotFound(format!(
                    "{} (index {})",
                    selector.name, selector.duplicate_index
                ))
                .to_string(),
            )
        })
}

fn collect_candidates() -> Result<Vec<DeviceCandidate<cpal::Device>>> {
    let records = collect_device_records()?;
    Ok(collect_candidates_from_records(records))
}

fn collect_device_records() -> Result<Vec<DeviceRecord<cpal::Device>>> {
    let host = get_audio_host()?;
    let devices = host.output_devices().map_err(Error::backend)?;
    let mut records = Vec::new();

    for device in devices {
        let Ok(name) = device.name() else {
            log::debug!("AVB: skipping audio output with unreadable name");
            continue;
        };

        let output_config_ranges = device
            .supported_output_configs()
            .map(|configs| {
                configs
                    .map(|cfg| OutputConfigRange {
                        channels: cfg.channels(),
                        min_sample_rate: cfg.min_sample_rate().0,
                        max_sample_rate: cfg.max_sample_rate().0,
                        sample_format: cfg.sample_format(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let default_config = device.default_output_config().ok();
        let default_output_channels = default_config.as_ref().map(|cfg| cfg.channels());
        let default_output_sample_rate = default_config.as_ref().map(|cfg| cfg.sample_rate().0);

        log::debug!(
            "AVB: found audio output {:?} — config ranges: [{}], default channels: {:?}, default sample rate: {:?}",
            name,
            output_config_ranges
                .iter()
                .map(|r| format!(
                    "{}ch {}-{}Hz",
                    r.channels, r.min_sample_rate, r.max_sample_rate
                ))
                .collect::<Vec<_>>()
                .join(", "),
            default_output_channels,
            default_output_sample_rate,
        );

        records.push(DeviceRecord {
            name,
            device,
            output_config_ranges,
            default_output_channels,
            default_output_sample_rate,
        });
    }

    log::debug!("AVB: enumerated {} audio output(s) total", records.len());
    Ok(records)
}

fn collect_candidates_from_records<D>(records: Vec<DeviceRecord<D>>) -> Vec<DeviceCandidate<D>> {
    let mut records: Vec<DeviceRecord<D>> = records
        .into_iter()
        .filter(|record| {
            if is_blacklisted_device(&record.name) {
                log::debug!(
                    "AVB: skipping {:?} — blacklisted (not a laser DAC)",
                    record.name
                );
                return false;
            }
            let channel_ok = supports_required_channels(record);
            if !channel_ok {
                log::debug!(
                    "AVB: skipping {:?} — insufficient channels (need >= {})",
                    record.name,
                    MIN_CHANNELS
                );
            }
            channel_ok
        })
        .collect();

    records.sort_by(|a, b| {
        normalize_device_name(&a.name)
            .cmp(&normalize_device_name(&b.name))
            .then(a.name.cmp(&b.name))
    });

    let mut per_name_counter: HashMap<String, u16> = HashMap::new();
    let mut indexed = Vec::with_capacity(records.len());

    for record in records {
        let key = normalize_device_name(&record.name);
        let duplicate_index = *per_name_counter.get(&key).unwrap_or(&0);
        per_name_counter.insert(key, duplicate_index.saturating_add(1));
        indexed.push(DeviceCandidate {
            selector: AvbSelector {
                name: record.name,
                duplicate_index,
            },
            device: record.device,
            output_config_ranges: record.output_config_ranges,
            default_output_sample_rate: record.default_output_sample_rate,
        });
    }

    indexed
}

fn supports_required_channels<D>(record: &DeviceRecord<D>) -> bool {
    if record
        .output_config_ranges
        .iter()
        .any(|cfg| cfg.channels >= MIN_CHANNELS)
    {
        return true;
    }

    record
        .default_output_channels
        .is_some_and(|channels| channels >= MIN_CHANNELS)
}

fn select_stream_config(candidate: &DeviceCandidate<cpal::Device>) -> Result<SelectedStreamConfig> {
    let (channels, sample_rate) = match choose_stream_config(
        &candidate.output_config_ranges,
        candidate.default_output_sample_rate,
    ) {
        Some(config) => {
            log::debug!(
                "AVB: selected {} channels at {}Hz for {:?}",
                config.0,
                config.1,
                candidate.selector.name
            );
            config
        }
        None => {
            log::error!(
                "AVB: no compatible output config for {:?} — need >= {} channels",
                candidate.selector.name,
                MIN_CHANNELS,
            );
            return Err(Error::backend(super::error::Error::UnsupportedOutputConfig));
        }
    };

    let sample_format =
        choose_sample_format(&candidate.output_config_ranges, channels, sample_rate);

    Ok(SelectedStreamConfig {
        channels,
        sample_rate,
        sample_format,
    })
}

/// Pick a sample format for the chosen (channels, rate), preferring f32 (native
/// output type) and falling back to i16/i32 for devices — many ALSA/ASIO
/// interfaces — that only expose integer formats. Defaults to f32 when no
/// range matches (the build will surface any real mismatch).
fn choose_sample_format(
    config_ranges: &[OutputConfigRange],
    channels: u16,
    sample_rate: u32,
) -> SampleFormat {
    let matching = || {
        config_ranges.iter().filter(move |r| {
            r.channels == channels && (r.min_sample_rate..=r.max_sample_rate).contains(&sample_rate)
        })
    };
    for preferred in [SampleFormat::F32, SampleFormat::I16, SampleFormat::I32] {
        if matching().any(|r| r.sample_format == preferred) {
            return preferred;
        }
    }
    // No preferred format matched exactly; take whatever the first matching
    // range offers, else fall back to f32.
    matching()
        .map(|r| r.sample_format)
        .next()
        .unwrap_or(SampleFormat::F32)
}

fn build_cpal_stream_config(stream_config: SelectedStreamConfig) -> cpal::StreamConfig {
    // Always let the host/driver pick the buffer size. Requesting a fixed
    // size fails on drivers that don't support it: ASIO drivers (e.g. RME)
    // only accept the buffer size configured in their own control panel, and
    // WASAPI shared mode can reject buffer durations that don't match the
    // engine period. The queue provides the jitter cushion, so the device
    // buffer size only affects callback granularity, not correctness.
    cpal::StreamConfig {
        channels: stream_config.channels,
        sample_rate: cpal::SampleRate(stream_config.sample_rate),
        buffer_size: cpal::BufferSize::Default,
    }
}

/// Choose the best (channels, sample_rate) pair from the available config ranges.
///
/// Prefers the default sample rate when it falls within a qualifying range,
/// otherwise falls back to the max sample rate from the best qualifying range.
/// Always picks the lowest channel count >= MIN_CHANNELS.
fn choose_stream_config(
    config_ranges: &[OutputConfigRange],
    default_sample_rate: Option<u32>,
) -> Option<(u16, u32)> {
    let qualifying: Vec<_> = config_ranges
        .iter()
        .filter(|r| r.channels >= MIN_CHANNELS)
        .collect();

    if qualifying.is_empty() {
        return None;
    }

    // Try the default sample rate first.
    if let Some(rate) = default_sample_rate {
        if let Some(best) = qualifying
            .iter()
            .filter(|r| (r.min_sample_rate..=r.max_sample_rate).contains(&rate))
            .min_by_key(|r| r.channels)
        {
            return Some((best.channels, rate));
        }
    }

    // Fallback: pick the lowest-channel qualifying range and use its max sample rate.
    qualifying
        .iter()
        .min_by_key(|r| r.channels)
        .map(|r| (r.channels, r.max_sample_rate))
}

/// Directly enqueue points without resampling. Test helper for exercising the
/// queue and `fill_output_buffer` in isolation.
#[cfg(test)]
fn enqueue_points(runtime: &RuntimeState, points: &[LaserPoint]) -> WriteOutcome {
    if !runtime.has_capacity_for(points.len()) {
        return WriteOutcome::WouldBlock;
    }

    for point in points {
        runtime.push_point(StreamPoint::from(point));
    }
    WriteOutcome::Written
}

impl From<&LaserPoint> for StreamPoint {
    fn from(point: &LaserPoint) -> Self {
        Self {
            x: point.x.clamp(-1.0, 1.0),
            y: point.y.clamp(-1.0, 1.0),
            r: scale_u16_to_f32(point.r),
            g: scale_u16_to_f32(point.g),
            b: scale_u16_to_f32(point.b),
            i: scale_u16_to_f32(point.intensity),
        }
    }
}

fn scale_u16_to_f32(value: u16) -> f32 {
    value as f32 / u16::MAX as f32
}

/// Fill an f32 output buffer. Kept as a concrete entry point for tests and the
/// fake engine; delegates to the format-generic implementation.
fn fill_output_buffer(data: &mut [f32], output_channels: usize, runtime: &RuntimeState) {
    fill_output_buffer_converted(data, output_channels, runtime);
}

/// Fill an output buffer of any cpal sample format from the runtime queue.
///
/// XY are sign-flipped to match the galvo convention. On a 5-channel (XYRGB)
/// config there is no dedicated intensity channel, so RGB is premultiplied by
/// the point intensity; on a 6-channel (XYRGBI) config intensity is written to
/// its own channel. On underrun the last XY position is held and RGB blanked
/// (laser-safe); on shutter-closed RGB is blanked but XY still tracks.
fn fill_output_buffer_converted<S: Sample + cpal::FromSample<f32>>(
    data: &mut [S],
    output_channels: usize,
    runtime: &RuntimeState,
) {
    let shutter_open = runtime.shutter_open.load(Ordering::Acquire);

    for frame in data.chunks_mut(output_channels) {
        let point = match runtime.pop_point() {
            Some(point) => {
                runtime.set_last_xy(point.x, point.y);
                point
            }
            None => {
                let (x, y) = runtime.last_xy();
                StreamPoint {
                    x,
                    y,
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    i: 0.0,
                }
            }
        };

        let mut vals = [0.0f32; CHANNELS_XYRGBI];
        vals[0] = -point.x;
        vals[1] = -point.y;
        if shutter_open && frame.len() >= CHANNELS_XYRGB {
            if frame.len() >= CHANNELS_XYRGBI {
                vals[2] = point.r;
                vals[3] = point.g;
                vals[4] = point.b;
                vals[5] = point.i;
            } else {
                // No dedicated intensity channel: fold intensity into RGB.
                vals[2] = point.r * point.i;
                vals[3] = point.g * point.i;
                vals[4] = point.b * point.i;
            }
        }

        for (i, slot) in frame.iter_mut().enumerate() {
            let v = if i < CHANNELS_XYRGBI { vals[i] } else { 0.0 };
            *slot = S::from_sample(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::thread;

    #[test]
    fn catmull_output_is_clamped_to_valid_ranges() {
        // The non-monotone Catmull-Rom spline overshoots its control points, so
        // in-range inputs can produce out-of-range interpolants. Emitted points
        // must still be clamped to XY ∈ [-1, 1] and colors/intensity ∈ [0, 1] so
        // an undershoot below 0 can't turn into a spurious positive in the
        // 5-channel `r * i` premultiply, and XY can't drive galvos past range.
        let p = |v: f32| StreamPoint {
            x: v,
            y: v,
            r: v,
            g: v,
            b: v,
            i: v,
        };

        // First arrangement (0,1,1,0) bulges above the upper bound; the second
        // (1,0,0,1) dips below the lower bound. Together they exercise both the
        // color/intensity undershoot and the XY over/undershoot.
        for &[a, b, c, d] in &[[0.0f32, 1.0, 1.0, 0.0], [1.0, 0.0, 0.0, 1.0]] {
            let (s0, s1, s2, s3) = (p(a), p(b), p(c), p(d));
            // Confirm the raw spline actually leaves range at the midpoint, so
            // this test would fail without the clamp.
            let raw = crate::resample::catmull_rom(a, b, c, d, 0.5);
            assert!(
                !(0.0..=1.0).contains(&raw),
                "control points must overshoot to make the clamp observable"
            );

            for k in 0..=20 {
                let t = k as f32 / 20.0;
                let out = StreamPoint::catmull(s0, s1, s2, s3, t);
                for v in [out.r, out.g, out.b, out.i] {
                    assert!((0.0..=1.0).contains(&v), "color/intensity {v} out of [0,1]");
                }
                assert!((-1.0..=1.0).contains(&out.x), "x {} out of [-1,1]", out.x);
                assert!((-1.0..=1.0).contains(&out.y), "y {} out of [-1,1]", out.y);
            }
        }
    }

    fn make_record(
        name: &str,
        channels: u16,
        min_sample_rate: u32,
        max_sample_rate: u32,
    ) -> DeviceRecord<()> {
        DeviceRecord {
            name: name.to_string(),
            device: (),
            output_config_ranges: vec![OutputConfigRange {
                channels,
                min_sample_rate,
                max_sample_rate,
                sample_format: SampleFormat::F32,
            }],
            default_output_channels: Some(channels),
            default_output_sample_rate: Some(max_sample_rate),
        }
    }

    struct FakeRunningStream {
        stop_tx: Option<mpsc::Sender<()>>,
        handle: Option<JoinHandle<()>>,
    }

    impl Drop for FakeRunningStream {
        fn drop(&mut self) {
            if let Some(tx) = self.stop_tx.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl RunningAudioStream for FakeRunningStream {}

    struct FakeAudioEngine {
        fail_open: AtomicBool,
        paused: Arc<AtomicBool>,
        captured_frames: Arc<Mutex<std::collections::VecDeque<Vec<f32>>>>,
        opened_stream_configs: Arc<Mutex<Vec<SelectedStreamConfig>>>,
        frame_budget: Arc<AtomicU32>,
        frame_count: Arc<std::sync::atomic::AtomicUsize>,
        channels: usize,
        sample_rate: AtomicU32,
        sample_rate_after_resolve: Option<u32>,
        same_name_count: Option<usize>,
    }

    impl FakeAudioEngine {
        fn new(channels: usize) -> Self {
            Self::with_sample_rate(channels, 48_000)
        }

        fn with_sample_rate(channels: usize, sample_rate: u32) -> Self {
            Self {
                fail_open: AtomicBool::new(false),
                paused: Arc::new(AtomicBool::new(false)),
                captured_frames: Arc::new(Mutex::new(std::collections::VecDeque::new())),
                opened_stream_configs: Arc::new(Mutex::new(Vec::new())),
                frame_budget: Arc::new(AtomicU32::new(u32::MAX)),
                frame_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                channels,
                sample_rate: AtomicU32::new(sample_rate),
                sample_rate_after_resolve: None,
                same_name_count: None,
            }
        }

        fn with_sample_rate_change_on_open(
            channels: usize,
            sample_rate: u32,
            next_sample_rate: u32,
        ) -> Self {
            Self {
                sample_rate_after_resolve: Some(next_sample_rate),
                ..Self::with_sample_rate(channels, sample_rate)
            }
        }

        fn with_same_name_count(mut self, count: usize) -> Self {
            self.same_name_count = Some(count);
            self
        }

        fn snapshot(&self) -> Vec<Vec<f32>> {
            self.captured_frames
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect()
        }

        fn frame_count(&self) -> usize {
            self.frame_count.load(Ordering::Acquire)
        }

        fn set_frame_budget(&self, frames: u32) {
            self.frame_budget.store(frames, Ordering::Release);
        }

        fn opened_stream_configs(&self) -> Vec<SelectedStreamConfig> {
            self.opened_stream_configs.lock().unwrap().clone()
        }
    }

    impl AudioEngine for FakeAudioEngine {
        fn resolve_stream_config(&self, _selector: &AvbSelector) -> Result<ResolvedConfig> {
            if self.fail_open.load(Ordering::Acquire) {
                return Err(Error::backend(
                    crate::protocols::avb::error::Error::StreamStartFailed,
                ));
            }
            let sample_rate = self.sample_rate.load(Ordering::Acquire);
            if let Some(next_sample_rate) = self.sample_rate_after_resolve {
                self.sample_rate.store(next_sample_rate, Ordering::Release);
            }
            Ok(ResolvedConfig {
                config: SelectedStreamConfig {
                    channels: self.channels as u16,
                    sample_rate,
                    sample_format: SampleFormat::F32,
                },
                same_name_count: self.same_name_count,
            })
        }

        fn open_stream(
            &self,
            _selector: &AvbSelector,
            stream_config: SelectedStreamConfig,
            runtime: Arc<RuntimeState>,
        ) -> Result<Box<dyn RunningAudioStream>> {
            if self.fail_open.load(Ordering::Acquire) {
                return Err(Error::backend(
                    crate::protocols::avb::error::Error::StreamStartFailed,
                ));
            }

            let (stop_tx, stop_rx) = mpsc::channel();
            let captured_frames = Arc::clone(&self.captured_frames);
            let frame_budget = Arc::clone(&self.frame_budget);
            let frame_count = Arc::clone(&self.frame_count);
            let paused = Arc::clone(&self.paused);
            let channels = stream_config.channels as usize;

            if let Ok(mut configs) = self.opened_stream_configs.lock() {
                configs.push(stream_config);
            }

            let handle = thread::spawn(move || loop {
                match stop_rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(_) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        if paused.load(Ordering::Acquire) {
                            continue;
                        }
                        if !try_acquire_frame_budget(&frame_budget) {
                            continue;
                        }
                        let mut frame = vec![0.0; channels];
                        fill_output_buffer(&mut frame, channels, &runtime);
                        if let Ok(mut captured) = captured_frames.lock() {
                            captured.push_back(frame);
                            if captured.len() > 2048 {
                                captured.pop_front();
                            }
                        }
                        frame_count.fetch_add(1, Ordering::Release);
                    }
                }
            });

            Ok(Box::new(FakeRunningStream {
                stop_tx: Some(stop_tx),
                handle: Some(handle),
            }))
        }
    }

    fn try_acquire_frame_budget(frame_budget: &AtomicU32) -> bool {
        loop {
            let current = frame_budget.load(Ordering::Acquire);
            if current == u32::MAX {
                return true;
            }
            if current == 0 {
                return false;
            }
            if frame_budget
                .compare_exchange(current, current - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn wait_for_frame_count(engine: &FakeAudioEngine, target: usize) {
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline {
            if engine.frame_count() >= target {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "timed out waiting for frame count {}, got {}",
            target,
            engine.frame_count()
        );
    }

    /// Engine whose `open_stream` blocks until the test releases it,
    /// simulating a driver call that hangs during stream startup. Once
    /// released it fails, so the worker thread exits promptly regardless of
    /// whether anyone is listening for its stop signal.
    struct BlockingOpenEngine {
        release_rx: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl AudioEngine for BlockingOpenEngine {
        fn resolve_stream_config(&self, _selector: &AvbSelector) -> Result<ResolvedConfig> {
            Ok(ResolvedConfig {
                config: SelectedStreamConfig {
                    channels: CHANNELS_XYRGBI as u16,
                    sample_rate: 48_000,
                    sample_format: SampleFormat::F32,
                },
                same_name_count: None,
            })
        }

        fn open_stream(
            &self,
            _selector: &AvbSelector,
            _stream_config: SelectedStreamConfig,
            _runtime: Arc<RuntimeState>,
        ) -> Result<Box<dyn RunningAudioStream>> {
            if let Some(rx) = self.release_rx.lock().unwrap().take() {
                let _ = rx.recv();
            }
            Err(Error::backend(
                crate::protocols::avb::error::Error::StreamStartFailed,
            ))
        }
    }

    #[test]
    fn build_cpal_stream_config_uses_default_buffer_size() {
        let config = build_cpal_stream_config(SelectedStreamConfig {
            channels: 6,
            sample_rate: 48_000,
            sample_format: SampleFormat::F32,
        });
        assert_eq!(config.buffer_size, cpal::BufferSize::Default);
        assert_eq!(config.channels, 6);
        assert_eq!(config.sample_rate, cpal::SampleRate(48_000));
    }

    #[test]
    fn connect_times_out_without_hanging_when_open_stream_blocks() {
        let (release_tx, release_rx) = mpsc::channel();
        let engine: Arc<dyn AudioEngine> = Arc::new(BlockingOpenEngine {
            release_rx: Mutex::new(Some(release_rx)),
        });
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);
        backend.init_timeout = Duration::from_millis(50);

        // Watchdog: if connect regresses to joining the blocked worker it
        // would deadlock (the release send below is sequenced after connect
        // returns). Releasing the engine after a few seconds makes
        // open_stream fail and the worker exit, so connect returns and the
        // elapsed-time assertion fails instead of hanging the test suite.
        let watchdog_tx = release_tx.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(3));
            let _ = watchdog_tx.send(());
        });

        let t0 = std::time::Instant::now();
        let err = backend.connect().unwrap_err();
        // Must return promptly instead of joining the blocked worker thread.
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert!(err
            .to_string()
            .contains("failed to start AVB output stream"));
        assert!(!backend.is_connected());

        // Unblock the detached worker so it exits instead of lingering for
        // the rest of the test run.
        let _ = release_tx.send(());
    }

    #[test]
    fn stream_point_from_laser_point_clamps_and_scales() {
        let point = LaserPoint::new(2.0, -2.0, 65535, 32768, 0, 65535);
        let mapped = StreamPoint::from(&point);
        assert_eq!(mapped.x, 1.0);
        assert_eq!(mapped.y, -1.0);
        assert_eq!(mapped.r, 1.0);
        assert!(mapped.g > 0.49 && mapped.g < 0.51);
        assert_eq!(mapped.b, 0.0);
        assert_eq!(mapped.i, 1.0);
    }

    #[test]
    fn enqueue_points_returns_would_block_when_capacity_exceeded() {
        let cap = queue_capacity_for_rate(48_000);
        let runtime = RuntimeState::new(true, 48_000);
        let points = vec![LaserPoint::blanked(0.0, 0.0); cap];
        assert_eq!(enqueue_points(&runtime, &points), WriteOutcome::Written);
        assert_eq!(
            enqueue_points(&runtime, &[LaserPoint::blanked(0.0, 0.0)]),
            WriteOutcome::WouldBlock
        );
    }

    #[test]
    fn collect_candidates_filters_and_assigns_duplicate_indices() {
        let records = vec![
            make_record("Built-in Output", 8, 44_100, 48_000),
            make_record("Broadcom NetXtreme A", 8, 44_100, 48_000),
            make_record("Broadcom NetXtreme A", 8, 44_100, 48_000),
            make_record("Broadcom NetXtreme B", 2, 44_100, 48_000),
            make_record("Broadcom NetXtreme B", 8, 44_100, 48_000),
            make_record("Studio Display Speakers", 6, 44_100, 48_000),
        ];

        let candidates = collect_candidates_from_records(records);
        let selectors: Vec<(String, u16)> = candidates
            .iter()
            .map(|c| (c.selector.name.clone(), c.selector.duplicate_index))
            .collect();

        assert_eq!(
            selectors,
            vec![
                ("Broadcom NetXtreme A".to_string(), 0),
                ("Broadcom NetXtreme A".to_string(), 1),
                ("Broadcom NetXtreme B".to_string(), 0),
                ("Built-in Output".to_string(), 0),
            ]
        );
    }

    #[test]
    fn supports_required_channels_uses_default_fallback() {
        let record = DeviceRecord {
            name: "Broadcom NetXtreme".to_string(),
            device: (),
            output_config_ranges: Vec::new(),
            default_output_channels: Some(8),
            default_output_sample_rate: Some(48_000),
        };
        assert!(supports_required_channels(&record));
    }

    #[test]
    fn fill_output_buffer_shutter_closed_blanks_rgbi_only() {
        let runtime = RuntimeState::new(false, 48_000);
        let points = [LaserPoint::new(0.25, -0.5, 65535, 32768, 1000, 65535)];
        assert_eq!(enqueue_points(&runtime, &points), WriteOutcome::Written);

        let mut data = vec![0.0; CHANNELS_XYRGBI];
        fill_output_buffer(&mut data, CHANNELS_XYRGBI, &runtime);

        assert!((data[0] + 0.25).abs() < 0.0001);
        assert!((data[1] - 0.5).abs() < 0.0001);
        assert_eq!(data[2], 0.0);
        assert_eq!(data[3], 0.0);
        assert_eq!(data[4], 0.0);
        assert_eq!(data[5], 0.0);
        assert_eq!(runtime.queued_points(), 0);
    }

    #[test]
    fn fill_output_buffer_underrun_outputs_zeroes() {
        let runtime = RuntimeState::new(true, 48_000);
        let mut data = vec![1.0; CHANNELS_XYRGBI * 2];
        fill_output_buffer(&mut data, CHANNELS_XYRGBI, &runtime);
        assert!(data.iter().all(|v| *v == 0.0));
        assert_eq!(runtime.queued_points(), 0);
    }

    #[test]
    fn fill_output_buffer_open_shutter_writes_full_channels() {
        let runtime = RuntimeState::new(true, 48_000);
        let point = LaserPoint::new(0.1, 0.2, 65535, 0, 65535, 32768);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0.0; CHANNELS_XYRGBI];
        fill_output_buffer(&mut data, CHANNELS_XYRGBI, &runtime);

        assert!((data[0] + 0.1).abs() < 0.0001);
        assert!((data[1] + 0.2).abs() < 0.0001);
        assert_eq!(data[2], 1.0);
        assert_eq!(data[3], 0.0);
        assert_eq!(data[4], 1.0);
        assert!(data[5] > 0.49 && data[5] < 0.51);
    }

    #[test]
    fn underrun_holds_last_xy_blanked() {
        let runtime = RuntimeState::new(true, 48_000);
        let point = LaserPoint::new(0.3, -0.4, 65535, 1000, 0, 65535);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0.0; CHANNELS_XYRGBI * 2];
        fill_output_buffer(&mut data, CHANNELS_XYRGBI, &runtime);

        // First frame: queued point.
        assert!((data[0] + 0.3).abs() < 0.0001);
        assert!((data[1] - 0.4).abs() < 0.0001);
        assert!(data[2] > 0.0);
        assert!(data[5] > 0.0);

        // Second frame: underrun; keep XY, blank colors/intensity.
        assert!((data[6] + 0.3).abs() < 0.0001);
        assert!((data[7] - 0.4).abs() < 0.0001);
        assert_eq!(data[8], 0.0);
        assert_eq!(data[9], 0.0);
        assert_eq!(data[10], 0.0);
        assert_eq!(data[11], 0.0);
    }

    #[test]
    fn choose_stream_config_prefers_lowest_compatible_channel_count() {
        let ranges = vec![
            OutputConfigRange {
                channels: 8,
                min_sample_rate: 44_100,
                max_sample_rate: 96_000,
                sample_format: SampleFormat::F32,
            },
            OutputConfigRange {
                channels: 6,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
                sample_format: SampleFormat::F32,
            },
        ];
        assert_eq!(
            choose_stream_config(&ranges, Some(48_000)),
            Some((6, 48_000))
        );
    }

    #[test]
    fn choose_stream_config_uses_default_rate_when_in_range() {
        let ranges = vec![OutputConfigRange {
            channels: 8,
            min_sample_rate: 44_100,
            max_sample_rate: 96_000,
            sample_format: SampleFormat::F32,
        }];
        assert_eq!(
            choose_stream_config(&ranges, Some(96_000)),
            Some((8, 96_000))
        );
    }

    #[test]
    fn choose_stream_config_prefers_default_rate_when_supported() {
        let ranges = vec![OutputConfigRange {
            channels: 6,
            min_sample_rate: 48_000,
            max_sample_rate: 96_000,
            sample_format: SampleFormat::F32,
        }];
        assert_eq!(
            choose_stream_config(&ranges, Some(96_000)),
            Some((6, 96_000))
        );
    }

    #[test]
    fn choose_stream_config_falls_back_to_max_rate() {
        let ranges = vec![OutputConfigRange {
            channels: 8,
            min_sample_rate: 44_100,
            max_sample_rate: 44_100,
            sample_format: SampleFormat::F32,
        }];
        // Default rate 48kHz is not in range, so falls back to max_sample_rate.
        assert_eq!(
            choose_stream_config(&ranges, Some(48_000)),
            Some((8, 44_100))
        );
    }

    #[test]
    fn choose_stream_config_falls_back_when_default_rate_not_supported_with_required_channels() {
        let ranges = vec![
            OutputConfigRange {
                channels: 2,
                min_sample_rate: 48_000,
                max_sample_rate: 96_000,
                sample_format: SampleFormat::F32,
            },
            OutputConfigRange {
                channels: 6,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
                sample_format: SampleFormat::F32,
            },
        ];
        assert_eq!(
            choose_stream_config(&ranges, Some(96_000)),
            Some((6, 48_000))
        );
    }

    #[test]
    fn choose_stream_config_returns_none_without_qualifying_channels() {
        let ranges = vec![OutputConfigRange {
            channels: 2,
            min_sample_rate: 44_100,
            max_sample_rate: 96_000,
            sample_format: SampleFormat::F32,
        }];
        assert_eq!(choose_stream_config(&ranges, Some(48_000)), None);
    }

    #[test]
    fn choose_stream_config_selects_valid_channels_and_rate_from_same_range() {
        let ranges = vec![
            OutputConfigRange {
                channels: 8,
                min_sample_rate: 96_000,
                max_sample_rate: 96_000,
                sample_format: SampleFormat::F32,
            },
            OutputConfigRange {
                channels: 6,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
                sample_format: SampleFormat::F32,
            },
        ];
        assert_eq!(
            choose_stream_config(&ranges, Some(96_000)),
            Some((8, 96_000))
        );
    }

    #[test]
    fn fake_engine_connect_write_disconnect_end_to_end() {
        let fake_engine = Arc::new(FakeAudioEngine::new(CHANNELS_XYRGBI));
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();
        let points = vec![LaserPoint::new(0.25, -0.25, 65535, 0, 0, 65535)];
        let outcome = backend.try_write_points(48_000, &points).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        thread::sleep(Duration::from_millis(20));
        let frames = fake_engine.snapshot();
        assert!(!frames.is_empty());
        assert!(frames
            .iter()
            .any(|frame| frame.len() >= 6 && frame[2] > 0.0 && frame[5] > 0.0));

        backend.disconnect().unwrap();
        assert!(!backend.is_connected());
    }

    #[test]
    fn stream_failure_flag_surfaces_as_disconnected() {
        let fake = Arc::new(FakeAudioEngine::new(CHANNELS_XYRGBI));
        let engine: Arc<dyn AudioEngine> = fake.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);
        backend.connect().unwrap();
        assert!(backend.is_connected());

        // Simulate the cpal error callback marking the stream dead.
        backend.runtime.as_ref().unwrap().mark_stream_failed();

        // Writes now report disconnected so the driver's reconnect path runs.
        let err = backend
            .try_write_points(48_000, &[LaserPoint::blanked(0.0, 0.0)])
            .unwrap_err();
        assert!(err.to_string().to_lowercase().contains("stream failed"));
        assert!(!backend.is_connected());

        backend.disconnect().unwrap();
    }

    #[test]
    fn fill_output_buffer_converts_to_i16() {
        let runtime = RuntimeState::new(true, 48_000);
        let point = LaserPoint::new(0.5, -0.5, 65535, 0, 0, 65535);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0i16; CHANNELS_XYRGBI];
        fill_output_buffer_converted(&mut data, CHANNELS_XYRGBI, &runtime);

        // frame[0] = -x = -0.5 → strongly negative i16; frame[1] = -y = 0.5.
        assert!(data[0] < -10_000);
        assert!(data[1] > 10_000);
        // R at full scale → near i16::MAX.
        assert!(data[2] > 30_000);
    }

    #[test]
    fn connect_refuses_when_same_name_device_count_changed() {
        // Scanned with 2 same-named devices; engine now reports 1 → the
        // positional index may bind a different unit, so connect must refuse.
        let fake = Arc::new(FakeAudioEngine::new(CHANNELS_XYRGBI).with_same_name_count(1));
        let engine: Arc<dyn AudioEngine> = fake.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);
        backend.scan_duplicate_count = Some(2);

        let err = backend.connect().unwrap_err();
        assert!(err.to_string().to_lowercase().contains("count changed"));
        assert!(!backend.is_connected());
    }

    #[test]
    fn connect_allows_when_same_name_device_count_matches() {
        let fake = Arc::new(FakeAudioEngine::new(CHANNELS_XYRGBI).with_same_name_count(2));
        let engine: Arc<dyn AudioEngine> = fake.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);
        backend.scan_duplicate_count = Some(2);

        backend.connect().unwrap();
        assert!(backend.is_connected());
        backend.disconnect().unwrap();
    }

    #[test]
    fn fake_engine_open_failure_propagates_from_connect() {
        let fake_engine = Arc::new(FakeAudioEngine::new(CHANNELS_XYRGBI));
        fake_engine.fail_open.store(true, Ordering::Release);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();

        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        let err = backend.connect().unwrap_err();
        assert!(err
            .to_string()
            .contains("failed to start AVB output stream"));
        assert!(!backend.is_connected());
    }

    #[test]
    fn wouldblock_then_recover_after_callback_drains() {
        let fake_engine = Arc::new(FakeAudioEngine::new(CHANNELS_XYRGBI));
        fake_engine.paused.store(true, Ordering::Release);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

        let cap = queue_capacity_for_rate(48_000);
        let fill = vec![LaserPoint::blanked(0.0, 0.0); cap];
        let first = backend.try_write_points(48_000, &fill).unwrap();
        assert_eq!(first, WriteOutcome::Written);

        let blocked = backend
            .try_write_points(48_000, &[LaserPoint::blanked(0.0, 0.0)])
            .unwrap();
        assert_eq!(blocked, WriteOutcome::WouldBlock);

        fake_engine.paused.store(false, Ordering::Release);
        thread::sleep(Duration::from_millis(25));

        let recovered = backend
            .try_write_points(48_000, &[LaserPoint::blanked(0.0, 0.0)])
            .unwrap();
        assert_eq!(recovered, WriteOutcome::Written);

        backend.disconnect().unwrap();
    }

    #[test]
    fn callback_progresses_under_producer_contention() {
        let cap = queue_capacity_for_rate(48_000);
        let engine = FakeAudioEngine::new(CHANNELS_XYRGBI);
        let runtime = Arc::new(RuntimeState::new(true, 48_000));
        let selector = AvbSelector {
            name: "MOTU AVB Main".to_string(),
            duplicate_index: 0,
        };
        let _stream = engine
            .open_stream(
                &selector,
                SelectedStreamConfig {
                    channels: CHANNELS_XYRGBI as u16,
                    sample_rate: runtime.sample_rate,
                    sample_format: SampleFormat::F32,
                },
                Arc::clone(&runtime),
            )
            .expect("stream should open");

        let stop = Arc::new(AtomicBool::new(false));
        let stop_writer = Arc::clone(&stop);
        let runtime_writer = Arc::clone(&runtime);
        let writer = thread::spawn(move || {
            let points = vec![LaserPoint::new(0.2, -0.2, 65535, 65535, 0, 65535); 32];
            while !stop_writer.load(Ordering::Acquire) {
                let _ = enqueue_points(&runtime_writer, &points);
            }
        });

        thread::sleep(Duration::from_millis(30));
        let start_frames = engine.frame_count();
        thread::sleep(Duration::from_millis(30));
        let end_frames = engine.frame_count();

        stop.store(true, Ordering::Release);
        let _ = writer.join();

        assert!(end_frames > start_frames);
        assert!(runtime.queued_points() <= cap as u64);
    }

    #[test]
    fn queue_bound_invariant_under_stress() {
        let cap = queue_capacity_for_rate(48_000);
        let engine = FakeAudioEngine::new(CHANNELS_XYRGBI);
        let runtime = Arc::new(RuntimeState::new(true, 48_000));
        let selector = AvbSelector {
            name: "MOTU AVB Main".to_string(),
            duplicate_index: 0,
        };
        let _stream = engine
            .open_stream(
                &selector,
                SelectedStreamConfig {
                    channels: CHANNELS_XYRGBI as u16,
                    sample_rate: runtime.sample_rate,
                    sample_format: SampleFormat::F32,
                },
                Arc::clone(&runtime),
            )
            .expect("stream should open");

        for _ in 0..300 {
            let _ = enqueue_points(
                &runtime,
                &[LaserPoint::new(0.1, 0.1, 65535, 0, 0, 65535); 64],
            );
            assert!(runtime.queued_points() <= cap as u64);
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn disconnect_completes_under_load() {
        let fake_engine = Arc::new(FakeAudioEngine::new(CHANNELS_XYRGBI));
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        let runtime = backend.runtime.as_ref().unwrap().clone();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_writer = Arc::clone(&stop);
        let writer = thread::spawn(move || {
            let points = vec![LaserPoint::blanked(0.0, 0.0); 64];
            while !stop_writer.load(Ordering::Acquire) {
                let _ = enqueue_points(&runtime, &points);
            }
        });

        thread::sleep(Duration::from_millis(20));
        stop.store(true, Ordering::Release);

        let t0 = std::time::Instant::now();
        backend.disconnect().unwrap();
        let elapsed = t0.elapsed();
        let _ = writer.join();

        assert!(elapsed < Duration::from_secs(1));
    }

    #[test]
    fn resampler_write_interpolates_xy() {
        let fake = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 48_000));
        fake.paused.store(true, Ordering::Release);
        let engine: Arc<dyn AudioEngine> = fake.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);
        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535),
        ];
        // 24k → 48k, fresh phase: total_emittable(2) = floor(1*2)+1 = 3.
        assert_eq!(
            backend.try_write_points(24_000, &points).unwrap(),
            WriteOutcome::Written
        );
        let runtime = backend.runtime.as_ref().unwrap().clone();
        assert_eq!(runtime.queued_points(), 3);

        // First sample sits at the input start.
        let p0 = runtime.pop_point().unwrap();
        assert!((p0.x - (-1.0)).abs() < 0.01);
        assert!((p0.y - (-1.0)).abs() < 0.01);
        assert!(p0.r < 0.01);

        // Middle sample is interpolated in both position and color.
        let p1 = runtime.pop_point().unwrap();
        assert!(p1.x > -1.0 && p1.x < 1.0);
        assert!(p1.y > -1.0 && p1.y < 1.0);
        assert!(p1.r > 0.0 && p1.r < 1.0);

        // Last sample sits at the input end.
        let p2 = runtime.pop_point().unwrap();
        assert!((p2.x - 1.0).abs() < 0.01);
        assert!((p2.y - 1.0).abs() < 0.01);
        assert!(p2.r > 0.99);

        backend.disconnect().unwrap();
    }

    #[test]
    fn resampler_write_checks_capacity() {
        let cap = queue_capacity_for_rate(48_000);
        let fake = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 48_000));
        fake.paused.store(true, Ordering::Release);
        let engine: Arc<dyn AudioEngine> = fake.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);
        backend.connect().unwrap();

        // Fill the queue to one below capacity directly.
        let runtime = backend.runtime.as_ref().unwrap().clone();
        let fill = vec![LaserPoint::blanked(0.0, 0.0); cap - 1];
        assert_eq!(enqueue_points(&runtime, &fill), WriteOutcome::Written);

        // 2 points at 24k → 3 output samples, but only 1 slot remains.
        let points = vec![
            LaserPoint::new(0.0, 0.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 0, 0, 0, 0),
        ];
        assert_eq!(
            backend.try_write_points(24_000, &points).unwrap(),
            WriteOutcome::WouldBlock
        );

        backend.disconnect().unwrap();
    }

    #[test]
    fn engine_at_96khz_uses_correct_queue_capacity_and_resampling() {
        let fake_engine = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 96_000));
        fake_engine.paused.store(true, Ordering::Release);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();

        let runtime = backend.runtime.as_ref().unwrap().clone();
        assert_eq!(runtime.sample_rate, 96_000);
        assert_eq!(runtime.queue.capacity(), queue_capacity_for_rate(96_000));

        // Resampling 2 points at 48kHz to 96kHz with a fresh phase yields
        // total_emittable(2) = floor(1 * 96000/48000) + 1 = 3 output samples.
        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535),
        ];
        backend.set_shutter(true).unwrap();
        let outcome = backend.try_write_points(48_000, &points).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        assert_eq!(runtime.queued_points(), 3);

        backend.disconnect().unwrap();
    }

    #[test]
    fn try_write_points_passthrough_when_pps_matches_selected_audio_rate() {
        let fake_engine = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 96_000));
        fake_engine.paused.store(true, Ordering::Release);
        fake_engine.set_frame_budget(3);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(0.0, 0.0, 32768, 0, 0, 32768),
            LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535),
        ];
        assert_eq!(
            backend.try_write_points(96_000, &points).unwrap(),
            WriteOutcome::Written
        );

        let start_frames = fake_engine.frame_count();
        fake_engine.paused.store(false, Ordering::Release);
        wait_for_frame_count(fake_engine.as_ref(), start_frames + 3);
        fake_engine.paused.store(true, Ordering::Release);

        let frames = fake_engine.snapshot();
        let captured = &frames[frames.len() - 3..];
        assert_eq!(captured.len(), 3);
        assert!((captured[0][0] - 1.0).abs() < 0.01);
        assert!((captured[1][0] - 0.0).abs() < 0.01);
        assert!((captured[2][0] - (-1.0)).abs() < 0.01);

        backend.disconnect().unwrap();
    }

    #[test]
    fn try_write_points_resamples_up_to_selected_audio_rate() {
        let fake_engine = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 96_000));
        fake_engine.paused.store(true, Ordering::Release);
        fake_engine.set_frame_budget(3);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

        // 2 points at 48kHz → 96kHz, fresh phase → 3 output samples.
        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535),
        ];
        assert_eq!(
            backend.try_write_points(48_000, &points).unwrap(),
            WriteOutcome::Written
        );

        let start_frames = fake_engine.frame_count();
        fake_engine.paused.store(false, Ordering::Release);
        wait_for_frame_count(fake_engine.as_ref(), start_frames + 3);
        fake_engine.paused.store(true, Ordering::Release);

        let frames = fake_engine.snapshot();
        let captured = &frames[frames.len() - 3..];
        assert_eq!(captured.len(), 3);
        // frame[0] = -x: start (-1) → 1.0, interpolated mid, end (1) → -1.0.
        assert!((captured[0][0] - 1.0).abs() < 0.01);
        assert!(captured[1][0] > -1.0 && captured[1][0] < 1.0);
        assert!((captured[2][0] - (-1.0)).abs() < 0.01);

        backend.disconnect().unwrap();
    }

    #[test]
    fn try_write_points_resamples_down_to_selected_audio_rate() {
        let fake_engine = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 48_000));
        fake_engine.paused.store(true, Ordering::Release);
        fake_engine.set_frame_budget(3);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(-0.5, -0.5, 0, 0, 0, 0),
            LaserPoint::new(0.5, 0.5, 65535, 65535, 65535, 65535),
            LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535),
            LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535),
        ];
        assert_eq!(
            backend.try_write_points(96_000, &points).unwrap(),
            WriteOutcome::Written
        );

        let start_frames = fake_engine.frame_count();
        fake_engine.paused.store(false, Ordering::Release);
        wait_for_frame_count(fake_engine.as_ref(), start_frames + 3);
        fake_engine.paused.store(true, Ordering::Release);

        let frames = fake_engine.snapshot();
        let captured = &frames[frames.len() - 3..];
        assert_eq!(captured.len(), 3);
        assert!((captured[0][0] - 1.0).abs() < 0.01);
        assert!(captured[1][0] > -1.0 && captured[1][0] < 1.0);
        assert!((captured[2][0] - (-1.0)).abs() < 0.01);

        backend.disconnect().unwrap();
    }

    #[test]
    fn queue_capacity_scales_with_audio_sample_rate() {
        let fake_engine = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 96_000));
        fake_engine.paused.store(true, Ordering::Release);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();

        let cap = queue_capacity_for_rate(96_000);
        let fill = vec![LaserPoint::blanked(0.0, 0.0); cap];
        assert_eq!(
            backend.try_write_points(96_000, &fill).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(
            backend
                .try_write_points(96_000, &[LaserPoint::blanked(0.0, 0.0)])
                .unwrap(),
            WriteOutcome::WouldBlock
        );

        backend.disconnect().unwrap();
    }

    #[test]
    fn reconnect_rebuilds_runtime_for_new_audio_sample_rate() {
        let fake_engine = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 48_000));
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        let first_runtime = backend.runtime.as_ref().unwrap().clone();
        assert_eq!(first_runtime.sample_rate, 48_000);
        assert_eq!(
            first_runtime.queue.capacity(),
            queue_capacity_for_rate(48_000)
        );
        backend.disconnect().unwrap();

        fake_engine.sample_rate.store(96_000, Ordering::Release);

        backend.connect().unwrap();
        let second_runtime = backend.runtime.as_ref().unwrap().clone();
        assert_eq!(second_runtime.sample_rate, 96_000);
        assert_eq!(
            second_runtime.queue.capacity(),
            queue_capacity_for_rate(96_000)
        );
        backend.disconnect().unwrap();
    }

    #[test]
    fn fill_output_buffer_5ch_premultiplies_rgb_by_intensity() {
        let runtime = RuntimeState::new(true, 48_000);
        // Intensity 32768 ≈ 0.5: with no dedicated intensity channel, RGB is
        // folded by intensity so brightness is still honored on 5ch configs.
        let point = LaserPoint::new(0.1, 0.2, 65535, 0, 65535, 32768);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0.0; CHANNELS_XYRGB];
        fill_output_buffer(&mut data, CHANNELS_XYRGB, &runtime);

        assert!((data[0] + 0.1).abs() < 0.0001);
        assert!((data[1] + 0.2).abs() < 0.0001);
        assert!((data[2] - 0.5).abs() < 0.001); // R * intensity
        assert_eq!(data[3], 0.0); // G * intensity
        assert!((data[4] - 0.5).abs() < 0.001); // B * intensity
    }

    #[test]
    fn fill_output_buffer_6ch_keeps_dedicated_intensity_channel() {
        // On a 6ch config RGB stays full and intensity gets its own channel —
        // contrast with the 5ch premultiply above.
        let runtime = RuntimeState::new(true, 48_000);
        let point = LaserPoint::new(0.1, 0.2, 65535, 0, 65535, 32768);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0.0; CHANNELS_XYRGBI];
        fill_output_buffer(&mut data, CHANNELS_XYRGBI, &runtime);

        assert_eq!(data[2], 1.0); // R at full scale
        assert_eq!(data[3], 0.0); // G
        assert_eq!(data[4], 1.0); // B at full scale
        assert!((data[5] - 0.5).abs() < 0.001); // intensity on its own channel
    }

    #[test]
    fn fill_output_buffer_5ch_shutter_closed_blanks_rgb() {
        let runtime = RuntimeState::new(false, 48_000);
        let points = [LaserPoint::new(0.25, -0.5, 65535, 32768, 1000, 65535)];
        assert_eq!(enqueue_points(&runtime, &points), WriteOutcome::Written);

        let mut data = vec![0.0; CHANNELS_XYRGB];
        fill_output_buffer(&mut data, CHANNELS_XYRGB, &runtime);

        assert!((data[0] + 0.25).abs() < 0.0001);
        assert!((data[1] - 0.5).abs() < 0.0001);
        assert_eq!(data[2], 0.0);
        assert_eq!(data[3], 0.0);
        assert_eq!(data[4], 0.0);
    }

    #[test]
    fn fill_output_buffer_5ch_underrun_holds_last_xy_blanked() {
        let runtime = RuntimeState::new(true, 48_000);
        let point = LaserPoint::new(0.3, -0.4, 65535, 1000, 0, 65535);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0.0; CHANNELS_XYRGB * 2];
        fill_output_buffer(&mut data, CHANNELS_XYRGB, &runtime);

        // First frame: queued point.
        assert!((data[0] + 0.3).abs() < 0.0001);
        assert!((data[1] - 0.4).abs() < 0.0001);
        assert!(data[2] > 0.0); // R

        // Second frame: underrun; keep XY, blank colors.
        assert!((data[5] + 0.3).abs() < 0.0001);
        assert!((data[6] - 0.4).abs() < 0.0001);
        assert_eq!(data[7], 0.0);
        assert_eq!(data[8], 0.0);
        assert_eq!(data[9], 0.0);
    }

    #[test]
    fn fake_engine_5ch_connect_write_disconnect_end_to_end() {
        let fake_engine = Arc::new(FakeAudioEngine::new(CHANNELS_XYRGB));
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();
        let points = vec![LaserPoint::new(0.25, -0.25, 65535, 0, 0, 65535)];
        let outcome = backend.try_write_points(48_000, &points).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        thread::sleep(Duration::from_millis(20));
        let frames = fake_engine.snapshot();
        assert!(!frames.is_empty());
        // 5-channel frames should have RGB but no intensity channel
        assert!(frames
            .iter()
            .any(|frame| frame.len() == 5 && frame[2] > 0.0));

        backend.disconnect().unwrap();
        assert!(!backend.is_connected());
    }

    #[test]
    fn choose_stream_config_selects_5ch_when_lowest_qualifying() {
        let ranges = vec![
            OutputConfigRange {
                channels: 8,
                min_sample_rate: 44_100,
                max_sample_rate: 96_000,
                sample_format: SampleFormat::F32,
            },
            OutputConfigRange {
                channels: 5,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
                sample_format: SampleFormat::F32,
            },
        ];
        assert_eq!(
            choose_stream_config(&ranges, Some(48_000)),
            Some((5, 48_000))
        );
    }

    #[test]
    fn connect_and_open_use_same_stream_config_when_rate_changes_mid_connect() {
        let fake_engine = Arc::new(FakeAudioEngine::with_sample_rate_change_on_open(
            CHANNELS_XYRGBI,
            48_000,
            96_000,
        ));
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();

        let runtime = backend.runtime.as_ref().unwrap().clone();
        let opened_configs = fake_engine.opened_stream_configs();

        assert_eq!(runtime.sample_rate, 48_000);
        assert_eq!(runtime.queue.capacity(), queue_capacity_for_rate(48_000));
        assert_eq!(
            opened_configs,
            vec![SelectedStreamConfig {
                channels: CHANNELS_XYRGBI as u16,
                sample_rate: 48_000,
                sample_format: SampleFormat::F32,
            }]
        );

        backend.disconnect().unwrap();
    }
}
