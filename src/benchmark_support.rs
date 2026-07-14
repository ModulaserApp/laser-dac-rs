//! Hardware-free fixtures used by the benchmark targets.
//!
//! This module is available only with the `testutils` feature. Its small
//! interface keeps benchmark-only access to presentation and resampling
//! internals out of the normal crate surface.

use std::time::Duration;

use crate::config::IdlePolicy;
use crate::point::LaserPoint;
use crate::presentation::{
    default_transition, ColorDelayLine, Frame, PresentationEngine, SlicePipeline, TransitionFn,
    TransitionPlan,
};

/// Transition behavior used by presentation benchmark fixtures.
#[derive(Debug, Clone, Copy)]
pub enum TransitionWorkload {
    /// Coalesce every seam. Measures copying and lifecycle overhead without
    /// constructing transition points.
    Coalesce,
    /// Run the production transition implementation at the supplied PPS.
    Default { pps: u32 },
}

impl TransitionWorkload {
    fn build(self) -> TransitionFn {
        match self {
            Self::Coalesce => Box::new(|_, _| TransitionPlan::Coalesce),
            Self::Default { pps } => default_transition(pps),
        }
    }
}

/// Reusable fixture for `PresentationEngine` benchmarks.
pub struct PresentationBenchmark {
    engine: PresentationEngine,
}

impl PresentationBenchmark {
    pub fn new(initial: Frame, transition: TransitionWorkload) -> Self {
        let mut engine = PresentationEngine::new(transition.build());
        engine.set_pending(initial);
        Self { engine }
    }

    pub fn queue_frame(&mut self, frame: Frame) {
        self.engine.set_pending(frame);
    }

    pub fn set_frame_capacity(&mut self, capacity: Option<usize>) {
        self.engine.set_frame_capacity(capacity);
    }

    pub fn fill_chunk(&mut self, output: &mut [LaserPoint]) -> usize {
        self.engine.fill_chunk(output, output.len())
    }

    pub fn compose_hardware_frame(&mut self) -> &[LaserPoint] {
        self.engine.compose_hardware_frame()
    }
}

/// Reusable fixture for the full frame presentation pipeline.
pub struct SlicePipelineBenchmark {
    pipeline: SlicePipeline,
}

impl SlicePipelineBenchmark {
    pub fn new(
        frame: Frame,
        transition: TransitionWorkload,
        color_delay_points: usize,
        initial_capacity: usize,
    ) -> Self {
        let engine = PresentationEngine::new(transition.build());
        let mut pipeline = SlicePipeline::with_startup_blank(
            engine,
            color_delay_points,
            None,
            IdlePolicy::Blank,
            initial_capacity,
            Duration::ZERO,
        );
        pipeline.set_pending(frame);
        Self { pipeline }
    }

    pub fn queue_frame(&mut self, frame: Frame) {
        self.pipeline.set_pending(frame);
    }

    pub fn produce_fifo_chunk(&mut self, target_points: usize, pps: u32) -> &[LaserPoint] {
        self.pipeline.produce_fifo_chunk(target_points, pps, true)
    }

    pub fn produce_frame_swap(&mut self, pps: u32) -> &[LaserPoint] {
        self.pipeline.produce_frame_swap(pps, true)
    }
}

/// Reusable fixture for the color delay line on its own, with no composition
/// underneath it.
///
/// `SlicePipelineBenchmark` can only expose delay cost as the difference between
/// two cases that both compose a frame, so attributing a change to the delay
/// means subtracting one noisy number from another. This fixture applies the
/// delay and nothing else.
///
/// `apply` runs over the same buffer repeatedly, so colors shift along it and
/// carry recycles through it rather than replaying pristine input. That is
/// deliberate: refreshing the buffer would put a copy of the whole chunk inside
/// the measured closure, which is the exact cost being measured. The delay never
/// branches on a color value, so the work per call does not depend on the values
/// that drift.
pub struct ColorDelayBenchmark {
    line: ColorDelayLine,
    buf: Vec<LaserPoint>,
}

impl ColorDelayBenchmark {
    pub fn new(delay_points: usize, points: &[LaserPoint]) -> Self {
        Self {
            line: ColorDelayLine::new(delay_points),
            buf: points.to_vec(),
        }
    }

    pub fn apply(&mut self) -> &[LaserPoint] {
        self.line.apply(&mut self.buf);
        &self.buf
    }
}

/// Which DAC's driver-thread work a [`ChunkPipelineBenchmark`] reproduces.
///
/// Each variant carries the buffer discipline that DAC's backend actually uses,
/// including copies and per-chunk allocations that [`ProtocolEncoderBenchmark`]
/// deliberately engineers away in order to isolate encoding.
#[cfg(feature = "chunk-pipeline-bench")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkTarget {
    /// Frame-swap. Composes, converts and encodes on the driver thread.
    Helios,
    /// FIFO. Converts into the command queue's staging buffer and encodes, all
    /// on the driver thread.
    EtherDream,
    /// FIFO. Converts on the driver thread, then hands the buffer to a worker
    /// thread that encodes. Encoding is *not* driver-thread work.
    Idn,
    /// FIFO. Same driver/worker split as [`ChunkTarget::Idn`].
    LaserCubeNetwork,
    /// FIFO. Converts and encodes on the driver thread.
    LaserCubeUsb,
}

#[cfg(feature = "chunk-pipeline-bench")]
impl ChunkTarget {
    fn is_frame_swap(self) -> bool {
        matches!(self, Self::Helios)
    }
}

/// The conversion and encoding a backend performs *on the driver thread*,
/// modelling each protocol's real buffer reuse.
///
/// Only the buffers a given protocol actually keeps are present, so a fixture
/// cannot accidentally measure a buffer discipline its backend does not have.
#[cfg(feature = "chunk-pipeline-bench")]
enum DriverThreadEncoder {
    Helios {
        points: Vec<crate::protocols::helios::Point>,
        bytes: Vec<u8>,
    },
    EtherDream {
        /// Models `dac::stream::Stream::point_buffer`, which `CommandQueue::data`
        /// converts into directly — the backend keeps no second buffer of its own.
        points: Vec<crate::protocols::ether_dream::DacPoint>,
        bytes: Vec<u8>,
    },
    Idn {
        points: Vec<crate::protocols::idn::PointXyrgbi>,
    },
    LaserCubeNetwork {
        points: Vec<crate::protocols::lasercube_network::BenchmarkPoint>,
    },
    LaserCubeUsb {
        points: Vec<crate::protocols::lasercube_usb::Sample>,
        bytes: Vec<u8>,
    },
}

#[cfg(feature = "chunk-pipeline-bench")]
impl DriverThreadEncoder {
    fn new(target: ChunkTarget, max_points: usize) -> Self {
        // Byte capacity is per-protocol; 20 bytes/point covers the widest wire
        // format (Ether Dream's 18) with room for headers.
        let byte_capacity = max_points.saturating_mul(20).saturating_add(16);
        match target {
            ChunkTarget::Helios => Self::Helios {
                points: Vec::with_capacity(max_points),
                bytes: Vec::with_capacity(byte_capacity),
            },
            ChunkTarget::EtherDream => Self::EtherDream {
                points: Vec::with_capacity(max_points),
                bytes: Vec::with_capacity(byte_capacity),
            },
            // IDN and LaserCube-network start empty on purpose: their backends
            // hand each chunk's allocation to a worker thread, so the steady
            // state is an empty buffer that regrows on every write.
            ChunkTarget::Idn => Self::Idn { points: Vec::new() },
            ChunkTarget::LaserCubeNetwork => Self::LaserCubeNetwork { points: Vec::new() },
            ChunkTarget::LaserCubeUsb => Self::LaserCubeUsb {
                points: Vec::with_capacity(max_points),
                bytes: Vec::with_capacity(byte_capacity),
            },
        }
    }

    /// Run one chunk's driver-thread conversion and encoding. Returns the
    /// number of driver-thread output units — bytes where the driver encodes,
    /// converted points where encoding belongs to a worker thread.
    fn encode(&mut self, slice: &[LaserPoint], pps: u32) -> usize {
        match self {
            Self::Helios { points, bytes } => {
                use crate::protocols::helios::{encode_frame_into, Point, WriteFrameFlags};

                points.clear();
                points.extend(slice.iter().map(Point::from));
                // `encode_frame_into` clears `bytes` itself.
                encode_frame_into(pps, points, WriteFrameFlags::SINGLE_MODE, bytes);
                bytes.len()
            }
            Self::EtherDream { points, bytes } => {
                use std::borrow::Cow;

                use crate::protocols::ether_dream::protocol::command::Data;
                use crate::protocols::ether_dream::{DacPoint, WriteBytes};

                // `CommandQueue::data` converts into the stream's staging buffer,
                // which `submit` then encodes — one pass over each point, not two.
                points.clear();
                points.extend(slice.iter().map(DacPoint::from));
                bytes.clear();
                bytes
                    .write_bytes(Data {
                        points: Cow::Borrowed(points),
                    })
                    .expect("writing to Vec<u8> cannot fail");
                bytes.len()
            }
            Self::Idn { points } => {
                use crate::protocols::idn::PointXyrgbi;

                points.clear();
                points.extend(slice.iter().map(PointXyrgbi::from));
                // The backend hands this allocation to its worker via
                // `mem::take`, so the next chunk grows a fresh one. Dropping
                // the taken buffer here stands in for the worker consuming it.
                let handed_off = std::mem::take(points);
                handed_off.len()
            }
            Self::LaserCubeNetwork { points } => {
                use crate::protocols::lasercube_network::BenchmarkPoint;

                points.clear();
                points.extend(slice.iter().map(BenchmarkPoint::from));
                let handed_off = std::mem::take(points);
                handed_off.len()
            }
            Self::LaserCubeUsb { points, bytes } => {
                use crate::protocols::lasercube_usb::Sample;

                points.clear();
                points.extend(slice.iter().map(Sample::from));
                bytes.clear();
                for point in points.iter() {
                    bytes.extend_from_slice(&point.to_bytes());
                }
                bytes.len()
            }
        }
    }
}

/// One driver iteration's worth of CPU for a specific DAC: produce a slice,
/// then run the conversion and encoding that DAC's backend performs on the
/// driver thread.
///
/// This is the highest-level hardware-free fixture in the suite. It exists
/// because no single benchmark otherwise corresponds to a unit of work the
/// driver actually performs: [`SlicePipelineBenchmark`] stops at the slice and
/// [`ProtocolEncoderBenchmark`] starts from one, and composing their numbers
/// misses the copies and allocations that only appear when the two halves are
/// joined by a real backend.
///
/// Drive it at the DAC's real `max_points_per_chunk` and read the result **per
/// chunk**, not per point: a chunk is what has to fit inside the driver's
/// cadence, and at IDN's 179 points the fixed per-chunk cost dominates.
///
/// Excluded, because they are I/O or scheduling rather than CPU: the adapter's
/// pacing sleeps, buffer-estimator queries (which call `Instant::now`), the
/// mpsc control drain, metrics atomics, and the device write itself.
#[cfg(feature = "chunk-pipeline-bench")]
pub struct ChunkPipelineBenchmark {
    pipeline: SlicePipeline,
    encoder: DriverThreadEncoder,
    target: ChunkTarget,
}

#[cfg(feature = "chunk-pipeline-bench")]
impl ChunkPipelineBenchmark {
    pub fn new(
        frame: Frame,
        transition: TransitionWorkload,
        color_delay_points: usize,
        chunk_points: usize,
        target: ChunkTarget,
    ) -> Self {
        let mut engine = PresentationEngine::new(transition.build());
        if target.is_frame_swap() {
            engine.set_frame_capacity(Some(chunk_points));
        }
        let mut pipeline = SlicePipeline::with_startup_blank(
            engine,
            color_delay_points,
            None,
            IdlePolicy::Blank,
            chunk_points,
            Duration::ZERO,
        );
        pipeline.set_pending(frame);
        Self {
            pipeline,
            encoder: DriverThreadEncoder::new(target, chunk_points),
            target,
        }
    }

    pub fn queue_frame(&mut self, frame: Frame) {
        self.pipeline.set_pending(frame);
    }

    pub fn is_frame_swap(&self) -> bool {
        self.target.is_frame_swap()
    }

    /// Produce one chunk and run its driver-thread conversion and encoding.
    /// `target_points` is ignored for frame-swap targets, which are sized by
    /// the composed frame rather than by a chunk request.
    pub fn step(&mut self, target_points: usize, pps: u32) -> usize {
        let slice = if self.target.is_frame_swap() {
            self.pipeline.produce_frame_swap(pps, true)
        } else {
            self.pipeline.produce_fifo_chunk(target_points, pps, true)
        };
        self.encoder.encode(slice, pps)
    }
}

/// Reusable stereo resampler fixture for the oscilloscope backend.
///
/// The oscilloscope backend resamples `(f32, f32)` (XY only). The AVB backend
/// does *not* use this type — see [`AvbResamplerBenchmark`] for that path.
#[cfg(feature = "oscilloscope")]
pub struct StereoResamplerBenchmark {
    resampler: crate::resample::StreamingResampler<(f32, f32)>,
    output: Vec<(f32, f32)>,
}

#[cfg(feature = "oscilloscope")]
impl StereoResamplerBenchmark {
    pub fn new(from_rate: u32, to_rate: u32, output_capacity: usize) -> Self {
        Self {
            resampler: crate::resample::StreamingResampler::new(from_rate, to_rate),
            output: Vec::with_capacity(output_capacity),
        }
    }

    /// Process one input chunk while preserving phase between calls.
    pub fn process(&mut self, input: &[(f32, f32)]) -> &[(f32, f32)] {
        self.output.clear();
        self.resampler
            .process(input, |point| self.output.push(point));
        &self.output
    }
}

/// Reusable resampler fixture mirroring the AVB backend's write path.
///
/// AVB resamples a 6-channel `StreamPoint` (XYRGBI), so its per-sample cost is
/// materially higher than a 2-channel stereo resample. [`Self::process`] repeats
/// the work `AvbBackend::try_write_points` performs per chunk: re-phase to the
/// current PPS, size the output, convert `LaserPoint` into a retained scratch
/// buffer, then stream through the resampler.
///
/// The real backend emits into a lock-free `ArrayQueue` consumed by the audio
/// callback; this fixture emits into a `Vec` so the measurement stays
/// hardware-free and single-threaded. Queue contention is not represented.
#[cfg(feature = "avb")]
pub struct AvbResamplerBenchmark {
    resampler: crate::resample::StreamingResampler<crate::protocols::avb::BenchmarkStreamPoint>,
    scratch: Vec<crate::protocols::avb::BenchmarkStreamPoint>,
    sink: Vec<crate::protocols::avb::BenchmarkStreamPoint>,
}

#[cfg(feature = "avb")]
impl AvbResamplerBenchmark {
    pub fn new(pps: u32, sample_rate: u32, output_capacity: usize) -> Self {
        Self {
            resampler: crate::resample::StreamingResampler::new(pps.max(1), sample_rate),
            scratch: Vec::with_capacity(output_capacity),
            sink: Vec::with_capacity(output_capacity),
        }
    }

    /// Convert and resample one chunk, preserving phase between calls.
    pub fn process(&mut self, points: &[LaserPoint], pps: u32, sample_rate: u32) -> usize {
        use crate::protocols::avb::BenchmarkStreamPoint;

        self.resampler.set_rates(pps.max(1), sample_rate);
        let expected = self.resampler.pending_output_count(points.len());

        self.scratch.clear();
        self.scratch
            .extend(points.iter().map(BenchmarkStreamPoint::from));
        self.sink.clear();
        let scratch = std::mem::take(&mut self.scratch);
        self.resampler
            .process(&scratch, |point| self.sink.push(point));
        self.scratch = scratch;

        debug_assert_eq!(self.sink.len(), expected);
        self.sink.len()
    }
}

/// Reusable conversion and wire-encoding buffers for protocol benchmarks.
///
/// Each method measures the complete CPU path from `LaserPoint` conversion to
/// encoded bytes. Buffers are retained between calls so warmed benchmarks can
/// detect accidental allocations in code that is intended to reuse memory.
pub struct ProtocolEncoderBenchmark {
    output: Vec<u8>,
    #[cfg(feature = "helios")]
    helios_points: Vec<crate::protocols::helios::Point>,
    #[cfg(feature = "ether-dream")]
    ether_dream_points: Vec<crate::protocols::ether_dream::DacPoint>,
    #[cfg(feature = "idn")]
    idn_points: Vec<crate::protocols::idn::PointXyrgbi>,
    #[cfg(feature = "lasercube-network")]
    lasercube_network_points: Vec<crate::protocols::lasercube_network::BenchmarkPoint>,
    #[cfg(feature = "lasercube-usb")]
    lasercube_usb_points: Vec<crate::protocols::lasercube_usb::Sample>,
}

impl ProtocolEncoderBenchmark {
    pub fn new(max_points: usize) -> Self {
        Self {
            output: Vec::with_capacity(max_points.saturating_mul(20).saturating_add(16)),
            #[cfg(feature = "helios")]
            helios_points: Vec::with_capacity(max_points),
            #[cfg(feature = "ether-dream")]
            ether_dream_points: Vec::with_capacity(max_points),
            #[cfg(feature = "idn")]
            idn_points: Vec::with_capacity(max_points),
            #[cfg(feature = "lasercube-network")]
            lasercube_network_points: Vec::with_capacity(max_points),
            #[cfg(feature = "lasercube-usb")]
            lasercube_usb_points: Vec::with_capacity(max_points),
        }
    }

    #[cfg(feature = "helios")]
    pub fn encode_helios(&mut self, points: &[LaserPoint], pps: u32) -> &[u8] {
        use crate::protocols::helios::{encode_frame_into, WriteFrameFlags};

        self.helios_points.clear();
        self.helios_points
            .extend(points.iter().map(crate::protocols::helios::Point::from));
        encode_frame_into(
            pps,
            &self.helios_points,
            WriteFrameFlags::SINGLE_MODE,
            &mut self.output,
        );
        &self.output
    }

    #[cfg(feature = "ether-dream")]
    pub fn encode_ether_dream(&mut self, points: &[LaserPoint]) -> &[u8] {
        use std::borrow::Cow;

        use crate::protocols::ether_dream::protocol::command::Data;
        use crate::protocols::ether_dream::WriteBytes;

        self.ether_dream_points.clear();
        self.ether_dream_points.extend(
            points
                .iter()
                .map(crate::protocols::ether_dream::DacPoint::from),
        );
        self.output.clear();
        self.output
            .write_bytes(Data {
                points: Cow::Borrowed(&self.ether_dream_points),
            })
            .expect("writing to Vec<u8> cannot fail");
        &self.output
    }

    #[cfg(feature = "idn")]
    pub fn encode_idn_xyrgbi(&mut self, points: &[LaserPoint]) -> &[u8] {
        use crate::protocols::idn::protocol::Point as IdnPoint;
        use crate::protocols::idn::PointXyrgbi;

        self.idn_points.clear();
        self.idn_points.extend(points.iter().map(PointXyrgbi::from));
        self.output.clear();
        <PointXyrgbi as IdnPoint>::encode_batch_into(&self.idn_points, &mut self.output);
        &self.output
    }

    #[cfg(feature = "lasercube-network")]
    pub fn encode_lasercube_network(&mut self, points: &[LaserPoint]) -> &[u8] {
        use crate::protocols::lasercube_network::benchmark_encode_sample_packet;

        self.lasercube_network_points.clear();
        self.lasercube_network_points.extend(
            points
                .iter()
                .map(crate::protocols::lasercube_network::BenchmarkPoint::from),
        );
        benchmark_encode_sample_packet(1, 1, &self.lasercube_network_points, &mut self.output)
            .expect("benchmark input must fit one LaserCube packet");
        &self.output
    }

    #[cfg(feature = "lasercube-usb")]
    pub fn encode_lasercube_usb(&mut self, points: &[LaserPoint]) -> &[u8] {
        self.lasercube_usb_points.clear();
        self.lasercube_usb_points.extend(
            points
                .iter()
                .map(crate::protocols::lasercube_usb::Sample::from),
        );
        self.output.clear();
        for point in &self.lasercube_usb_points {
            self.output.extend_from_slice(&point.to_bytes());
        }
        &self.output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(n: usize) -> Vec<LaserPoint> {
        (0..n)
            .map(|i| {
                let x = i as f32 / n.max(1) as f32 * 2.0 - 1.0;
                LaserPoint::new(x, -x, 65535, 32768, 4096, 65535)
            })
            .collect()
    }

    #[test]
    fn presentation_fixture_produces_requested_chunk() {
        let frame = Frame::new(points(17));
        let mut fixture = PresentationBenchmark::new(frame, TransitionWorkload::Coalesce);
        let mut output = vec![LaserPoint::default(); 128];
        assert_eq!(fixture.fill_chunk(&mut output), output.len());
    }

    #[test]
    fn slice_pipeline_fixture_produces_frame_swap_frame() {
        let frame = Frame::new(points(64));
        let mut fixture =
            SlicePipelineBenchmark::new(frame, TransitionWorkload::Default { pps: 30_000 }, 5, 64);
        assert!(!fixture.produce_frame_swap(30_000).is_empty());
    }

    #[cfg(feature = "avb")]
    #[test]
    fn avb_resampler_fixture_upsamples_and_carries_phase() {
        let input = points(256);
        let mut fixture = AvbResamplerBenchmark::new(30_000, 48_000, 1_024);

        let first = fixture.process(&input, 30_000, 48_000);
        assert!(first > input.len(), "30k->48k must upsample");

        // Phase carries across calls, so an identical second chunk emits a
        // comparable count rather than restarting from a fresh phase.
        let second = fixture.process(&input, 30_000, 48_000);
        assert!(second.abs_diff(first) <= 1);
    }

    #[cfg(feature = "chunk-pipeline-bench")]
    #[test]
    fn chunk_pipeline_fixture_produces_driver_thread_output_per_target() {
        let frame = Frame::new(points(500));
        for target in [
            ChunkTarget::Helios,
            ChunkTarget::EtherDream,
            ChunkTarget::Idn,
            ChunkTarget::LaserCubeNetwork,
            ChunkTarget::LaserCubeUsb,
        ] {
            let mut fixture = ChunkPipelineBenchmark::new(
                frame.clone(),
                TransitionWorkload::Default { pps: 30_000 },
                0,
                179,
                target,
            );
            assert!(
                fixture.step(179, 30_000) > 0,
                "{target:?} must produce driver-thread output"
            );
        }
    }

    #[cfg(feature = "chunk-pipeline-bench")]
    #[test]
    fn chunk_pipeline_fixture_reports_bytes_or_points_per_target() {
        let frame = Frame::new(points(500));
        let fifo = |target| {
            let mut fixture = ChunkPipelineBenchmark::new(
                frame.clone(),
                TransitionWorkload::Coalesce,
                0,
                179,
                target,
            );
            fixture.step(179, 30_000)
        };

        // Where the driver thread encodes, the count is bytes on the wire.
        assert_eq!(fifo(ChunkTarget::EtherDream), 3 + 179 * 18);
        assert_eq!(fifo(ChunkTarget::LaserCubeUsb), 179 * 8);
        // Where encoding belongs to a worker thread, it is converted points.
        assert_eq!(fifo(ChunkTarget::Idn), 179);
        assert_eq!(fifo(ChunkTarget::LaserCubeNetwork), 179);
    }

    /// The IDN and LaserCube-network backends hand each chunk's conversion
    /// buffer to a worker thread, so the driver thread regrows it every write.
    /// The fixture must reproduce that rather than reusing a warm buffer, or it
    /// measures a buffer discipline those backends do not have.
    #[cfg(feature = "chunk-pipeline-bench")]
    #[test]
    fn chunk_pipeline_fixture_models_worker_handoff_allocation() {
        let frame = Frame::new(points(500));
        let mut fixture = ChunkPipelineBenchmark::new(
            frame,
            TransitionWorkload::Coalesce,
            0,
            179,
            ChunkTarget::Idn,
        );
        for _ in 0..3 {
            assert_eq!(fixture.step(179, 30_000), 179);
        }
        match &fixture.encoder {
            DriverThreadEncoder::Idn { points } => assert_eq!(
                points.capacity(),
                0,
                "buffer must be handed off, leaving nothing to reuse next chunk"
            ),
            _ => panic!("expected the IDN encoder"),
        }
    }

    #[cfg(feature = "helios")]
    #[test]
    fn helios_fixture_encodes_complete_frame() {
        let input = points(500);
        let mut fixture = ProtocolEncoderBenchmark::new(input.len());
        assert_eq!(fixture.encode_helios(&input, 30_000).len(), 500 * 7 + 5);
    }

    #[cfg(feature = "ether-dream")]
    #[test]
    fn ether_dream_fixture_encodes_data_command() {
        let input = points(500);
        let mut fixture = ProtocolEncoderBenchmark::new(input.len());
        assert_eq!(fixture.encode_ether_dream(&input).len(), 3 + 500 * 18);
    }

    #[cfg(feature = "idn")]
    #[test]
    fn idn_fixture_encodes_xyrgbi_points() {
        let input = points(179);
        let mut fixture = ProtocolEncoderBenchmark::new(input.len());
        assert_eq!(fixture.encode_idn_xyrgbi(&input).len(), 179 * 8);
    }

    #[cfg(feature = "lasercube-network")]
    #[test]
    fn lasercube_network_fixture_encodes_packet() {
        let input = points(140);
        let mut fixture = ProtocolEncoderBenchmark::new(input.len());
        assert_eq!(fixture.encode_lasercube_network(&input).len(), 4 + 140 * 10);
    }

    #[cfg(feature = "lasercube-usb")]
    #[test]
    fn lasercube_usb_fixture_encodes_samples() {
        let input = points(500);
        let mut fixture = ProtocolEncoderBenchmark::new(input.len());
        assert_eq!(fixture.encode_lasercube_usb(&input).len(), 500 * 8);
    }
}
