//! AVB DAC streaming backend implementation.

use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::avb::{is_likely_avb_device_name, normalize_device_name};
use crate::types::{DacCapabilities, DacType, LaserPoint};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const REQUIRED_CHANNELS: u16 = 6;
const FIXED_SAMPLE_RATE: u32 = 48_000;
// 9_600 points = 200ms at 48kHz.
// Chosen as a conservative jitter cushion while keeping queueing latency bounded.
const MAX_QUEUE_POINTS: usize = 9_600;
const MAPPED_CHANNELS: usize = 6;

/// Returns the platform-appropriate cpal audio host.
///
/// - macOS: default host (CoreAudio)
/// - Windows: ASIO host (required for reliable 6-channel output)
/// - Linux: default host (ALSA)
fn get_audio_host() -> Result<cpal::Host> {
    #[cfg(target_os = "macos")]
    {
        Ok(cpal::default_host())
    }
    #[cfg(target_os = "windows")]
    {
        cpal::host_from_id(cpal::HostId::Asio).map_err(|e| {
            Error::invalid_config(format!(
                "ASIO host not available (is the ASIO SDK installed?): {}",
                e
            ))
        })
    }
    #[cfg(target_os = "linux")]
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

struct DeviceRecord<D> {
    name: String,
    device: D,
    output_config_ranges: Vec<OutputConfigRange>,
    default_output_channels: Option<u16>,
}

struct DeviceCandidate<D> {
    selector: AvbSelector,
    device: D,
    output_config_ranges: Vec<OutputConfigRange>,
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
    shutter_open: AtomicBool,
    last_x_bits: AtomicU32,
    last_y_bits: AtomicU32,
}

impl RuntimeState {
    fn new(shutter_open: bool) -> Self {
        Self {
            queue: ArrayQueue::new(MAX_QUEUE_POINTS),
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
        MAX_QUEUE_POINTS.saturating_sub(self.queue.len())
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
    fn open_stream(
        &self,
        selector: &AvbSelector,
        runtime: Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>>;
}

struct CpalRunningStream {
    _stream: cpal::Stream,
}

impl RunningAudioStream for CpalRunningStream {}

struct CpalAudioEngine;

impl AudioEngine for CpalAudioEngine {
    fn open_stream(
        &self,
        selector: &AvbSelector,
        runtime: Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>> {
        let selected = select_device(selector)?;
        let stream_config = select_stream_config(&selected)?;
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
    pub fn new(name: String, duplicate_index: u16) -> Self {
        Self {
            selector: AvbSelector {
                name,
                duplicate_index,
            },
            runtime: None,
            stop_tx: None,
            worker_handle: None,
            engine: Arc::new(CpalAudioEngine),
            caps: super::default_capabilities(),
            desired_shutter_open: false,
        }
    }

    pub(crate) fn from_selector(selector: AvbSelector) -> Self {
        Self {
            selector,
            runtime: None,
            stop_tx: None,
            worker_handle: None,
            engine: Arc::new(CpalAudioEngine),
            caps: super::default_capabilities(),
            desired_shutter_open: false,
        }
    }

    #[cfg(test)]
    fn with_engine_for_test(
        name: String,
        duplicate_index: u16,
        engine: Arc<dyn AudioEngine>,
    ) -> Self {
        Self {
            selector: AvbSelector {
                name,
                duplicate_index,
            },
            runtime: None,
            stop_tx: None,
            worker_handle: None,
            engine,
            caps: super::default_capabilities(),
            desired_shutter_open: false,
        }
    }
}

impl StreamBackend for AvbBackend {
    fn dac_type(&self) -> DacType {
        DacType::Avb
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        if self.is_connected() {
            return Ok(());
        }

        let runtime = Arc::new(RuntimeState::new(self.desired_shutter_open));
        let runtime_for_worker = Arc::clone(&runtime);
        let selector = self.selector.clone();
        let engine = Arc::clone(&self.engine);

        let (stop_tx, stop_rx) = mpsc::channel();
        let (init_tx, init_rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            run_audio_worker(engine, selector, runtime_for_worker, stop_rx, init_tx);
        });

        match init_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                self.runtime = Some(runtime);
                self.stop_tx = Some(stop_tx);
                self.worker_handle = Some(handle);
                Ok(())
            }
            Ok(Err(err)) => {
                let _ = handle.join();
                Err(err)
            }
            Err(_) => {
                let _ = handle.join();
                Err(Error::backend(super::error::Error::StreamStartFailed))
            }
        }
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }

        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }

        if let Some(runtime) = self.runtime.take() {
            runtime.clear_queue();
        }

        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.runtime.is_some() && self.worker_handle.is_some()
    }

    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        if pps != FIXED_SAMPLE_RATE {
            return Err(Error::invalid_config(format!(
                "AVB backend requires {} PPS, got {}",
                FIXED_SAMPLE_RATE, pps
            )));
        }

        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        if points.is_empty() {
            return Ok(WriteOutcome::Written);
        }

        Ok(enqueue_points(runtime, points))
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

    fn queued_points(&self) -> Option<u64> {
        // This is host-side queue depth only (pre-callback ring),
        // not audio driver/AVB hardware buffer occupancy.
        self.runtime.as_ref().map(|rt| rt.queued_points())
    }
}

pub fn discover_device_selectors() -> Result<Vec<AvbSelector>> {
    let candidates = collect_candidates()?;
    Ok(candidates.into_iter().map(|c| c.selector).collect())
}

fn run_audio_worker(
    engine: Arc<dyn AudioEngine>,
    selector: AvbSelector,
    runtime: Arc<RuntimeState>,
    stop_rx: mpsc::Receiver<()>,
    init_tx: mpsc::Sender<Result<()>>,
) {
    let stream = match engine.open_stream(&selector, Arc::clone(&runtime)) {
        Ok(stream) => stream,
        Err(err) => {
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

    runtime.clear_queue();
    runtime.shutter_open.store(false, Ordering::Release);
    drop(stream);
}

fn select_device(selector: &AvbSelector) -> Result<DeviceCandidate<cpal::Device>> {
    let candidates = collect_candidates()?;
    candidates
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

        let default_output_channels = device
            .default_output_config()
            .ok()
            .map(|cfg| cfg.channels());

        records.push(DeviceRecord {
            name,
            device,
            output_config_ranges,
            default_output_channels,
        });
    }

    Ok(records)
}

fn collect_candidates_from_records<D>(records: Vec<DeviceRecord<D>>) -> Vec<DeviceCandidate<D>> {
    let mut records: Vec<DeviceRecord<D>> = records
        .into_iter()
        .filter(|record| {
            is_likely_avb_device_name(&record.name) && supports_required_channels(record)
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
        });
    }

    indexed
}

fn supports_required_channels<D>(record: &DeviceRecord<D>) -> bool {
    if record
        .output_config_ranges
        .iter()
        .any(|cfg| cfg.channels >= REQUIRED_CHANNELS)
    {
        return true;
    }

    record
        .default_output_channels
        .is_some_and(|channels| channels >= REQUIRED_CHANNELS)
}

fn select_stream_config(candidate: &DeviceCandidate<cpal::Device>) -> Result<cpal::StreamConfig> {
    let channels = match choose_stream_channels(&candidate.output_config_ranges) {
        Some(channels) => channels,
        None => return Err(Error::backend(super::error::Error::UnsupportedOutputConfig)),
    };

    let buffer_size = if cfg!(target_os = "windows") {
        cpal::BufferSize::Fixed(256)
    } else {
        cpal::BufferSize::Default
    };

    Ok(cpal::StreamConfig {
        channels,
        sample_rate: cpal::SampleRate(FIXED_SAMPLE_RATE),
        buffer_size,
    })
}

fn choose_stream_channels(config_ranges: &[OutputConfigRange]) -> Option<u16> {
    let mut channel_choice: Option<u16> = None;

    for range in config_ranges {
        if range.channels < REQUIRED_CHANNELS {
            continue;
        }
        if (range.min_sample_rate..=range.max_sample_rate).contains(&FIXED_SAMPLE_RATE) {
            channel_choice = Some(
                channel_choice
                    .map(|value| value.min(range.channels))
                    .unwrap_or(range.channels),
            );
        }
    }

    channel_choice
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

        let point = maybe_point.map_or_else(
            || {
                let (x, y) = runtime.last_xy();
                StreamPoint {
                    x,
                    y,
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    i: 0.0,
                }
            },
            |point| {
                runtime.set_last_xy(point.x, point.y);
                point
            },
        );

        if !frame.is_empty() {
            frame[0] = point.x;
        }
        if frame.len() > 1 {
            frame[1] = point.y;
        }
        if frame.len() >= MAPPED_CHANNELS {
            if shutter_open {
                frame[2] = point.r;
                frame[3] = point.g;
                frame[4] = point.b;
                frame[5] = point.i;
            } else {
                frame[2] = 0.0;
                frame[3] = 0.0;
                frame[4] = 0.0;
                frame[5] = 0.0;
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
        captured_frames: Arc<Mutex<Vec<Vec<f32>>>>,
        frame_count: Arc<std::sync::atomic::AtomicUsize>,
        channels: usize,
    }

    impl FakeAudioEngine {
        fn new(channels: usize) -> Self {
            Self {
                fail_open: AtomicBool::new(false),
                paused: Arc::new(AtomicBool::new(false)),
                captured_frames: Arc::new(Mutex::new(Vec::new())),
                frame_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                channels,
            }
        }

        fn snapshot(&self) -> Vec<Vec<f32>> {
            self.captured_frames
                .lock()
                .map(|frames| frames.clone())
                .unwrap_or_default()
        }

        fn frame_count(&self) -> usize {
            self.frame_count.load(Ordering::Acquire)
        }
    }

    impl AudioEngine for FakeAudioEngine {
        fn open_stream(
            &self,
            _selector: &AvbSelector,
            runtime: Arc<RuntimeState>,
        ) -> Result<Box<dyn RunningAudioStream>> {
            if self.fail_open.load(Ordering::Acquire) {
                return Err(Error::backend(
                    crate::protocols::avb::error::Error::StreamStartFailed,
                ));
            }

            let (stop_tx, stop_rx) = mpsc::channel();
            let captured_frames = Arc::clone(&self.captured_frames);
            let frame_count = Arc::clone(&self.frame_count);
            let paused = Arc::clone(&self.paused);
            let channels = self.channels;

            let handle = thread::spawn(move || loop {
                match stop_rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(_) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        if paused.load(Ordering::Acquire) {
                            continue;
                        }
                        let mut frame = vec![0.0; channels];
                        fill_output_buffer(&mut frame, channels, &runtime);
                        if let Ok(mut captured) = captured_frames.lock() {
                            captured.push(frame);
                            if captured.len() > 2048 {
                                captured.remove(0);
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
        let runtime = RuntimeState::new(true);
        let points = vec![LaserPoint::blanked(0.0, 0.0); MAX_QUEUE_POINTS];
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
            make_record("MOTU AVB Main", 8, 44_100, 48_000),
            make_record("MOTU AVB Main", 8, 44_100, 48_000),
            make_record("RME Digiface AVB", 2, 44_100, 48_000),
            make_record("RME Digiface AVB", 8, 44_100, 48_000),
        ];

        let candidates = collect_candidates_from_records(records);
        let selectors: Vec<(String, u16)> = candidates
            .iter()
            .map(|c| (c.selector.name.clone(), c.selector.duplicate_index))
            .collect();

        assert_eq!(
            selectors,
            vec![
                ("MOTU AVB Main".to_string(), 0),
                ("MOTU AVB Main".to_string(), 1),
                ("RME Digiface AVB".to_string(), 0),
            ]
        );
    }

    #[test]
    fn supports_required_channels_uses_default_fallback() {
        let record = DeviceRecord {
            name: "MOTU AVB Main".to_string(),
            device: (),
            output_config_ranges: Vec::new(),
            default_output_channels: Some(8),
        };
        assert!(supports_required_channels(&record));
    }

    #[test]
    fn fill_output_buffer_shutter_closed_blanks_rgbi_only() {
        let runtime = RuntimeState::new(false);
        let points = [LaserPoint::new(0.25, -0.5, 65535, 32768, 1000, 65535)];
        assert_eq!(enqueue_points(&runtime, &points), WriteOutcome::Written);

        let mut data = vec![0.0; MAPPED_CHANNELS];
        fill_output_buffer(&mut data, MAPPED_CHANNELS, &runtime);

        assert!((data[0] - 0.25).abs() < 0.0001);
        assert!((data[1] + 0.5).abs() < 0.0001);
        assert_eq!(data[2], 0.0);
        assert_eq!(data[3], 0.0);
        assert_eq!(data[4], 0.0);
        assert_eq!(data[5], 0.0);
        assert_eq!(runtime.queued_points(), 0);
    }

    #[test]
    fn fill_output_buffer_underrun_outputs_zeroes() {
        let runtime = RuntimeState::new(true);
        let mut data = vec![1.0; MAPPED_CHANNELS * 2];
        fill_output_buffer(&mut data, MAPPED_CHANNELS, &runtime);
        assert!(data.iter().all(|v| *v == 0.0));
        assert_eq!(runtime.queued_points(), 0);
    }

    #[test]
    fn fill_output_buffer_open_shutter_writes_full_channels() {
        let runtime = RuntimeState::new(true);
        let point = LaserPoint::new(0.1, 0.2, 65535, 0, 65535, 32768);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0.0; MAPPED_CHANNELS];
        fill_output_buffer(&mut data, MAPPED_CHANNELS, &runtime);

        assert!((data[0] - 0.1).abs() < 0.0001);
        assert!((data[1] - 0.2).abs() < 0.0001);
        assert_eq!(data[2], 1.0);
        assert_eq!(data[3], 0.0);
        assert_eq!(data[4], 1.0);
        assert!(data[5] > 0.49 && data[5] < 0.51);
    }

    #[test]
    fn underrun_holds_last_xy_blanked() {
        let runtime = RuntimeState::new(true);
        let point = LaserPoint::new(0.3, -0.4, 65535, 1000, 0, 65535);
        assert_eq!(enqueue_points(&runtime, &[point]), WriteOutcome::Written);

        let mut data = vec![0.0; MAPPED_CHANNELS * 2];
        fill_output_buffer(&mut data, MAPPED_CHANNELS, &runtime);

        // First frame: queued point.
        assert!((data[0] - 0.3).abs() < 0.0001);
        assert!((data[1] + 0.4).abs() < 0.0001);
        assert!(data[2] > 0.0);
        assert!(data[5] > 0.0);

        // Second frame: underrun; keep XY, blank colors/intensity.
        assert!((data[6] - 0.3).abs() < 0.0001);
        assert!((data[7] + 0.4).abs() < 0.0001);
        assert_eq!(data[8], 0.0);
        assert_eq!(data[9], 0.0);
        assert_eq!(data[10], 0.0);
        assert_eq!(data[11], 0.0);
    }

    #[test]
    fn choose_stream_channels_prefers_lowest_compatible_channel_count() {
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
        assert_eq!(choose_stream_channels(&ranges), Some(6));
    }

    #[test]
    fn choose_stream_channels_returns_none_without_48k_range() {
        let ranges = vec![OutputConfigRange {
            channels: 8,
            min_sample_rate: 44_100,
            max_sample_rate: 44_100,
        }];
        assert_eq!(choose_stream_channels(&ranges), None);
    }

    #[test]
    fn fake_engine_connect_write_disconnect_end_to_end() {
        let fake_engine = Arc::new(FakeAudioEngine::new(MAPPED_CHANNELS));
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();
        let points = vec![LaserPoint::new(0.25, -0.25, 65535, 0, 0, 65535)];
        let outcome = backend.try_write_chunk(FIXED_SAMPLE_RATE, &points).unwrap();
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
        let fake_engine = Arc::new(FakeAudioEngine::new(MAPPED_CHANNELS));
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
        let fake_engine = Arc::new(FakeAudioEngine::new(MAPPED_CHANNELS));
        fake_engine.paused.store(true, Ordering::Release);
        let engine: Arc<dyn AudioEngine> = fake_engine.clone();
        let mut backend = AvbBackend::with_engine_for_test("MOTU AVB Main".to_string(), 0, engine);

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

        let fill = vec![LaserPoint::blanked(0.0, 0.0); MAX_QUEUE_POINTS];
        let first = backend.try_write_chunk(FIXED_SAMPLE_RATE, &fill).unwrap();
        assert_eq!(first, WriteOutcome::Written);

        let blocked = backend
            .try_write_chunk(FIXED_SAMPLE_RATE, &[LaserPoint::blanked(0.0, 0.0)])
            .unwrap();
        assert_eq!(blocked, WriteOutcome::WouldBlock);

        fake_engine.paused.store(false, Ordering::Release);
        thread::sleep(Duration::from_millis(25));

        let recovered = backend
            .try_write_chunk(FIXED_SAMPLE_RATE, &[LaserPoint::blanked(0.0, 0.0)])
            .unwrap();
        assert_eq!(recovered, WriteOutcome::Written);

        backend.disconnect().unwrap();
    }

    #[test]
    fn callback_progresses_under_producer_contention() {
        let engine = FakeAudioEngine::new(MAPPED_CHANNELS);
        let runtime = Arc::new(RuntimeState::new(true));
        let selector = AvbSelector {
            name: "MOTU AVB Main".to_string(),
            duplicate_index: 0,
        };
        let _stream = engine
            .open_stream(&selector, Arc::clone(&runtime))
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
        assert!(runtime.queued_points() <= MAX_QUEUE_POINTS as u64);
    }

    #[test]
    fn queue_bound_invariant_under_stress() {
        let engine = FakeAudioEngine::new(MAPPED_CHANNELS);
        let runtime = Arc::new(RuntimeState::new(true));
        let selector = AvbSelector {
            name: "MOTU AVB Main".to_string(),
            duplicate_index: 0,
        };
        let _stream = engine
            .open_stream(&selector, Arc::clone(&runtime))
            .expect("stream should open");

        for _ in 0..300 {
            let _ = enqueue_points(
                &runtime,
                &[LaserPoint::new(0.1, 0.1, 65535, 0, 0, 65535); 64],
            );
            assert!(runtime.queued_points() <= MAX_QUEUE_POINTS as u64);
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn disconnect_completes_under_load() {
        let fake_engine = Arc::new(FakeAudioEngine::new(MAPPED_CHANNELS));
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
}
