//! AVB DAC streaming backend implementation.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::avb::{is_blacklisted_device, normalize_device_name};
use crate::resample::{catmull_rom, resampled_len};
use crate::types::{DacCapabilities, DacType, LaserPoint};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectedStreamConfig {
    channels: u16,
    sample_rate: u32,
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

#[derive(Debug, Clone)]
struct StreamPoint {
    x: f32,
    y: f32,
    r: f32,
    g: f32,
    b: f32,
    i: f32,
}

struct RuntimeState {
    queue: ArrayQueue<StreamPoint>,
    sample_rate: u32,
    shutter_open: AtomicBool,
    last_x_bits: AtomicU32,
    last_y_bits: AtomicU32,
}

impl RuntimeState {
    fn new(shutter_open: bool, sample_rate: u32) -> Self {
        Self {
            queue: ArrayQueue::new(queue_capacity_for_rate(sample_rate)),
            sample_rate,
            shutter_open: AtomicBool::new(shutter_open),
            last_x_bits: AtomicU32::new(0.0f32.to_bits()),
            last_y_bits: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    fn clear_queue(&self) {
        while self.queue.pop().is_some() {}
    }

    fn queued_points(&self) -> u64 {
        self.queue.len() as u64
    }

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

trait AudioEngine: Send + Sync {
    fn resolve_stream_config(&self, selector: &AvbSelector) -> Result<SelectedStreamConfig>;
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
    fn resolve_stream_config(&self, selector: &AvbSelector) -> Result<SelectedStreamConfig> {
        let candidate = select_device(selector)?;
        select_stream_config(&candidate)
    }

    fn open_stream(
        &self,
        selector: &AvbSelector,
        stream_config: SelectedStreamConfig,
        runtime: Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>> {
        let selected = select_device(selector)?;
        let stream_config = build_cpal_stream_config(stream_config);
        let output_channels = stream_config.channels as usize;
        let callback_state = Arc::clone(&runtime);

        let stream = selected
            .device
            .build_output_stream(
                &stream_config,
                move |data: &mut [f32], _| {
                    fill_output_buffer(data, output_channels, &callback_state);
                },
                move |err| {
                    log::error!("AVB output stream error: {}", err);
                },
                None,
            )
            .map_err(Error::backend)?;

        stream.play().map_err(Error::backend)?;

        Ok(Box::new(CpalRunningStream { _stream: stream }))
    }
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
        }
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

    pub(crate) fn from_selector(selector: AvbSelector) -> Self {
        Self::build(selector, Arc::new(CpalAudioEngine))
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
        let stream_config = self.engine.resolve_stream_config(&self.selector)?;
        log::info!(
            "AVB: selected {} channels at {}Hz",
            stream_config.channels,
            stream_config.sample_rate
        );
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

        match init_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                log::info!(
                    "AVB: connected to {:?} (duplicate_index={}) at {}Hz",
                    self.selector.name,
                    self.selector.duplicate_index,
                    stream_config.sample_rate
                );
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
            Err(_) => {
                log::error!("AVB: audio worker init timed out (5s)");
                let _ = handle.join();
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

        log::info!("AVB: disconnected from {:?}", self.selector.name);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.runtime.is_some() && self.worker_handle.is_some()
    }

    fn stop(&mut self) -> Result<()> {
        self.desired_shutter_open = false;
        if let Some(runtime) = &self.runtime {
            runtime.shutter_open.store(false, Ordering::Release);
            runtime.clear_queue();
        }
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

        if points.is_empty() {
            return Ok(WriteOutcome::Written);
        }

        // Fast path: no resampling needed when PPS matches the audio sample rate.
        if pps == runtime.sample_rate {
            Ok(enqueue_points(runtime, points))
        } else {
            Ok(enqueue_resampled(runtime, points, pps))
        }
    }

    fn queued_points(&self) -> Option<u64> {
        // This is host-side queue depth only (pre-callback ring),
        // not audio driver/AVB hardware buffer occupancy.
        self.runtime.as_ref().map(|rt| rt.queued_points())
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

    let _ = init_tx.send(Ok(()));

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

    Ok(SelectedStreamConfig {
        channels,
        sample_rate,
    })
}

fn build_cpal_stream_config(stream_config: SelectedStreamConfig) -> cpal::StreamConfig {
    let buffer_size = if cfg!(target_os = "windows") {
        cpal::BufferSize::Fixed(256)
    } else {
        cpal::BufferSize::Default
    };

    cpal::StreamConfig {
        channels: stream_config.channels,
        sample_rate: cpal::SampleRate(stream_config.sample_rate),
        buffer_size,
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

fn enqueue_points(runtime: &RuntimeState, points: &[LaserPoint]) -> WriteOutcome {
    if !runtime.has_capacity_for(points.len()) {
        return WriteOutcome::WouldBlock;
    }

    for point in points {
        runtime.push_point(StreamPoint::from(point));
    }
    WriteOutcome::Written
}

/// Interpolate all fields of four `StreamPoint`s using 4-point Catmull-Rom.
fn catmull_rom_stream_point(
    s0: &StreamPoint,
    s1: &StreamPoint,
    s2: &StreamPoint,
    s3: &StreamPoint,
    t: f32,
) -> StreamPoint {
    StreamPoint {
        x: catmull_rom(s0.x, s1.x, s2.x, s3.x, t),
        y: catmull_rom(s0.y, s1.y, s2.y, s3.y, t),
        r: catmull_rom(s0.r, s1.r, s2.r, s3.r, t),
        g: catmull_rom(s0.g, s1.g, s2.g, s3.g, t),
        b: catmull_rom(s0.b, s1.b, s2.b, s3.b, t),
        i: catmull_rom(s0.i, s1.i, s2.i, s3.i, t),
    }
}

/// Resample `points` from `pps` to `sample_rate` and enqueue directly.
///
/// Caller must ensure `points` is non-empty (the `try_write_points` fast path handles this).
fn enqueue_resampled(runtime: &RuntimeState, points: &[LaserPoint], pps: u32) -> WriteOutcome {
    debug_assert!(!points.is_empty());
    let output_len = resampled_len(points.len(), pps, runtime.sample_rate);
    if !runtime.has_capacity_for(output_len) {
        return WriteOutcome::WouldBlock;
    }

    let src: Vec<StreamPoint> = points.iter().map(StreamPoint::from).collect();
    let last_src_idx = (src.len() - 1) as f32;
    let step = if output_len > 1 {
        last_src_idx / (output_len - 1) as f32
    } else {
        0.0
    };

    let last = src.len() - 1;
    for i in 0..output_len {
        let src_pos = i as f32 * step;
        let idx = (src_pos as usize).min(last);
        let t = src_pos - idx as f32;
        let s0 = &src[idx.saturating_sub(1)];
        let s1 = &src[idx];
        let s2 = &src[(idx + 1).min(last)];
        let s3 = &src[(idx + 2).min(last)];
        runtime.push_point(catmull_rom_stream_point(s0, s1, s2, s3, t));
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

fn fill_output_buffer(data: &mut [f32], output_channels: usize, runtime: &RuntimeState) {
    let shutter_open = runtime.shutter_open.load(Ordering::Acquire);

    for frame in data.chunks_mut(output_channels) {
        frame.fill(0.0);
        let maybe_point = runtime.pop_point();

        let point = match maybe_point {
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

        if !frame.is_empty() {
            frame[0] = -point.x;
        }
        if frame.len() > 1 {
            frame[1] = -point.y;
        }
        if shutter_open && frame.len() >= CHANNELS_XYRGB {
            frame[2] = point.r;
            frame[3] = point.g;
            frame[4] = point.b;
            if frame.len() >= CHANNELS_XYRGBI {
                frame[5] = point.i;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::sync::Mutex;
    use std::thread;

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
        fn resolve_stream_config(&self, _selector: &AvbSelector) -> Result<SelectedStreamConfig> {
            if self.fail_open.load(Ordering::Acquire) {
                return Err(Error::backend(
                    crate::protocols::avb::error::Error::StreamStartFailed,
                ));
            }
            let sample_rate = self.sample_rate.load(Ordering::Acquire);
            if let Some(next_sample_rate) = self.sample_rate_after_resolve {
                self.sample_rate.store(next_sample_rate, Ordering::Release);
            }
            Ok(SelectedStreamConfig {
                channels: self.channels as u16,
                sample_rate,
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
            },
            OutputConfigRange {
                channels: 6,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
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
            },
            OutputConfigRange {
                channels: 6,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
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
            },
            OutputConfigRange {
                channels: 6,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
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
    fn enqueue_resampled_interpolates_xy() {
        let runtime = RuntimeState::new(true, 48_000);
        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535),
        ];
        // 2 points at 24k → ceil(2 * 48000 / 24000) = 4 output samples
        let outcome = enqueue_resampled(&runtime, &points, 24_000);
        assert_eq!(outcome, WriteOutcome::Written);
        assert_eq!(runtime.queued_points(), 4);

        // First sample should be at input start
        let p0 = runtime.pop_point().unwrap();
        assert!((p0.x - (-1.0)).abs() < 0.01);
        assert!((p0.y - (-1.0)).abs() < 0.01);
        assert!(p0.r < 0.01);

        // Middle samples should be interpolated (position and color)
        let p1 = runtime.pop_point().unwrap();
        assert!(p1.x > -1.0 && p1.x < 1.0);
        assert!(p1.y > -1.0 && p1.y < 1.0);
        assert!(p1.r > 0.0 && p1.r < 1.0);
        assert!(p1.g > 0.0 && p1.g < 1.0);

        let _p2 = runtime.pop_point().unwrap();

        // Last sample should be at input end
        let p3 = runtime.pop_point().unwrap();
        assert!((p3.x - 1.0).abs() < 0.01);
        assert!((p3.y - 1.0).abs() < 0.01);
        assert!(p3.r > 0.99);
    }

    #[test]
    fn enqueue_resampled_checks_capacity() {
        let cap = queue_capacity_for_rate(48_000);
        let runtime = RuntimeState::new(true, 48_000);
        // Fill the queue to near capacity
        let fill = vec![LaserPoint::blanked(0.0, 0.0); cap - 1];
        assert_eq!(enqueue_points(&runtime, &fill), WriteOutcome::Written);

        // 2 points at 24k → 4 output samples, but only 1 slot remains
        let points = vec![
            LaserPoint::new(0.0, 0.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 0, 0, 0, 0),
        ];
        let outcome = enqueue_resampled(&runtime, &points, 24_000);
        assert_eq!(outcome, WriteOutcome::WouldBlock);
    }

    #[test]
    fn engine_at_96khz_uses_correct_queue_capacity_and_resampling() {
        let fake_engine = Arc::new(FakeAudioEngine::with_sample_rate(CHANNELS_XYRGBI, 96_000));
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();

        let runtime = backend.runtime.as_ref().unwrap().clone();
        assert_eq!(runtime.sample_rate, 96_000);
        assert_eq!(runtime.queue.capacity(), queue_capacity_for_rate(96_000));

        // Resampling: 2 points at 48kHz → ceil(2 * 96000 / 48000) = 4 output samples
        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535),
        ];
        backend.set_shutter(true).unwrap();
        let outcome = backend.try_write_points(48_000, &points).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        assert_eq!(runtime.queued_points(), 4);

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
        fake_engine.set_frame_budget(4);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

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
        wait_for_frame_count(fake_engine.as_ref(), start_frames + 4);
        fake_engine.paused.store(true, Ordering::Release);

        let frames = fake_engine.snapshot();
        let captured = &frames[frames.len() - 4..];
        assert_eq!(captured.len(), 4);
        assert!((captured[0][0] - 1.0).abs() < 0.01);
        assert!(captured[1][0] > -1.0 && captured[1][0] < 1.0);
        assert!(captured[2][0] > -1.0 && captured[2][0] < 1.0);
        assert!((captured[3][0] - (-1.0)).abs() < 0.01);

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
    fn fill_output_buffer_5ch_open_shutter_writes_xyrgb_no_intensity() {
        let runtime = RuntimeState::new(true, 48_000);
        let point = LaserPoint::new(0.1, 0.2, 65535, 0, 65535, 32768);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0.0; CHANNELS_XYRGB];
        fill_output_buffer(&mut data, CHANNELS_XYRGB, &runtime);

        assert!((data[0] + 0.1).abs() < 0.0001);
        assert!((data[1] + 0.2).abs() < 0.0001);
        assert_eq!(data[2], 1.0); // R
        assert_eq!(data[3], 0.0); // G
        assert_eq!(data[4], 1.0); // B
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
            },
            OutputConfigRange {
                channels: 5,
                min_sample_rate: 48_000,
                max_sample_rate: 48_000,
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
            }]
        );

        backend.disconnect().unwrap();
    }
}
