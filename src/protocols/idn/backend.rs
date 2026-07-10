//! IDN DAC streaming backend implementation.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::{BufferEstimator, SoftwareDecayEstimator};
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;
use crate::protocols::idn::dac::stream::PointFormat;
use crate::protocols::idn::dac::{stream, ServerInfo, ServiceInfo};
use crate::protocols::idn::protocol::{PointExtended, PointXyrgbHighRes, PointXyrgbi};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const IDN_WORKER_QUEUE_CAPACITY: usize = 8;

/// How often the worker upgrades a data send to an ACK request (or, while
/// idle, sends a ping) to prove the device is still alive.
const ACK_CHECK_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for an ACK/ping response before counting it as a miss.
const ACK_TIMEOUT: Duration = Duration::from_millis(100);

/// Consecutive ACK/ping misses after which the device is considered dead and
/// the backend is marked disconnected so the driver's reconnect path engages.
const MAX_CONSECUTIVE_ACK_MISSES: u32 = 4;

/// Reachability check timeout used at `connect()`.
const CONNECT_PING_TIMEOUT: Duration = Duration::from_millis(300);

/// Number of extra ping retries at `connect()` before giving up.
const CONNECT_PING_RETRIES: u32 = 2;

struct WorkerRuntime {
    tx: mpsc::SyncSender<WorkerCommand>,
    connected: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// A chunk of converted points in the backend's selected wire format.
enum ChunkPoints {
    Xyrgbi(Vec<PointXyrgbi>),
    HighRes(Vec<PointXyrgbHighRes>),
    Extended(Vec<PointExtended>),
}

impl ChunkPoints {
    fn len(&self) -> usize {
        match self {
            ChunkPoints::Xyrgbi(v) => v.len(),
            ChunkPoints::HighRes(v) => v.len(),
            ChunkPoints::Extended(v) => v.len(),
        }
    }
}

struct QueuedChunk {
    pps: u32,
    points: ChunkPoints,
}

enum WorkerCommand {
    Chunk(QueuedChunk),
    Stop,
    Shutdown,
}

trait WorkerStream {
    fn scan_speed(&self) -> u32;
    fn set_scan_speed(&mut self, pps: u32);
    fn needs_keepalive(&self) -> bool;
    fn send_keepalive(&mut self) -> bool;
    fn write_frame(&mut self, points: &ChunkPoints) -> bool;
    /// Send the frame requesting an acknowledgment; returns whether the ACK
    /// was received within `timeout` (used for liveness detection).
    fn write_frame_with_ack(&mut self, points: &ChunkPoints, timeout: Duration) -> bool;
    /// Send a ping and report whether a response arrived within `timeout`.
    fn ping(&mut self, timeout: Duration) -> bool;
    fn close(&mut self);
}

impl WorkerStream for stream::Stream {
    fn scan_speed(&self) -> u32 {
        stream::Stream::scan_speed(self)
    }

    fn set_scan_speed(&mut self, pps: u32) {
        stream::Stream::set_scan_speed(self, pps);
    }

    fn needs_keepalive(&self) -> bool {
        stream::Stream::needs_keepalive(self)
    }

    fn send_keepalive(&mut self) -> bool {
        stream::Stream::send_keepalive(self).is_ok()
    }

    fn write_frame(&mut self, points: &ChunkPoints) -> bool {
        match points {
            ChunkPoints::Xyrgbi(v) => stream::Stream::write_frame(self, v).is_ok(),
            ChunkPoints::HighRes(v) => stream::Stream::write_frame(self, v).is_ok(),
            ChunkPoints::Extended(v) => stream::Stream::write_frame(self, v).is_ok(),
        }
    }

    fn write_frame_with_ack(&mut self, points: &ChunkPoints, timeout: Duration) -> bool {
        match points {
            ChunkPoints::Xyrgbi(v) => {
                stream::Stream::write_frame_with_ack(self, v, timeout).is_ok()
            }
            ChunkPoints::HighRes(v) => {
                stream::Stream::write_frame_with_ack(self, v, timeout).is_ok()
            }
            ChunkPoints::Extended(v) => {
                stream::Stream::write_frame_with_ack(self, v, timeout).is_ok()
            }
        }
    }

    fn ping(&mut self, timeout: Duration) -> bool {
        stream::Stream::ping(self, timeout).is_ok()
    }

    fn close(&mut self) {
        let _ = stream::Stream::close(self);
    }
}

/// IDN DAC backend (ILDA Digital Network).
pub struct IdnBackend {
    server: ServerInfo,
    service: ServiceInfo,
    runtime: Option<WorkerRuntime>,
    caps: DacCapabilities,
    /// Scratch conversion buffer. Its backing allocation is handed off to each
    /// queued chunk via `mem::take`, so a fresh allocation is grown on the next
    /// write; the field mainly keeps the conversion code allocation-free within
    /// a single write.
    point_buffer: Vec<PointXyrgbi>,
    /// Software-only buffer estimator. Driven by `record_send` from inside
    /// `try_write_points`; not yet consulted by the adapter (Phase 1).
    estimator: SoftwareDecayEstimator,
    /// Wire point format. Defaults to [`PointFormat::Xyrgbi`] (8-bit colour);
    /// callers can opt into a 16-bit format via [`IdnBackend::set_point_format`]
    /// before connecting.
    point_format: PointFormat,
}

impl IdnBackend {
    pub fn new(server: ServerInfo, service: ServiceInfo) -> Self {
        Self {
            server,
            service,
            runtime: None,
            caps: super::default_capabilities(),
            point_buffer: Vec::new(),
            estimator: SoftwareDecayEstimator::new(),
            point_format: PointFormat::Xyrgbi,
        }
    }

    /// Select the wire point format for streaming.
    ///
    /// Defaults to [`PointFormat::Xyrgbi`] (8-bit RGBI). Selecting
    /// [`PointFormat::XyrgbHighRes`] or [`PointFormat::Extended`] streams the
    /// crate's full 16-bit colour depth end-to-end, at the cost of larger
    /// samples (10 or 20 bytes vs 8) and therefore more packets per frame.
    ///
    /// Only receivers that advertise support for the chosen descriptors will
    /// render hi-res formats correctly, so the default is left at the
    /// universally understood XYRGBI format. Must be called **before**
    /// [`connect`](crate::backend::DacBackend::connect); changing it on a live
    /// connection has no effect until the next connect.
    pub fn set_point_format(&mut self, format: PointFormat) {
        self.point_format = format;
        self.caps.max_points_per_chunk = max_points_per_chunk_for(format);
    }

    /// The currently selected wire point format.
    pub fn point_format(&self) -> PointFormat {
        self.point_format
    }
}

/// Points that fit in one MTU-sized datagram (without a config header) for a
/// given format, used to keep the scheduler's chunk sizing aligned with the
/// sample size.
fn max_points_per_chunk_for(format: PointFormat) -> usize {
    use crate::protocols::idn::protocol::{
        ChannelMessageHeader, PacketHeader, SampleChunkHeader, SizeBytes, MAX_UDP_PAYLOAD,
    };
    let header =
        PacketHeader::SIZE_BYTES + ChannelMessageHeader::SIZE_BYTES + SampleChunkHeader::SIZE_BYTES;
    (MAX_UDP_PAYLOAD - header) / format.size_bytes()
}

impl DacBackend for IdnBackend {
    fn dac_type(&self) -> DacType {
        DacType::Idn
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        if self.runtime.is_some() {
            return Ok(());
        }

        let mut stream =
            stream::connect(&self.server, self.service.service_id).map_err(Error::backend)?;
        stream.set_frame_mode(stream::FrameMode::Wave);
        stream.set_point_format(self.point_format);

        // Validate reachability before committing: a plain UDP connect always
        // "succeeds", so without this a dead address would appear connected and
        // stream into the void. A ping (short timeout, a couple of retries)
        // confirms the server is actually answering.
        let mut reachable = false;
        for attempt in 0..=CONNECT_PING_RETRIES {
            match stream.ping(CONNECT_PING_TIMEOUT) {
                Ok(_) => {
                    reachable = true;
                    break;
                }
                Err(e) => log::debug!("idn connect: ping attempt {} failed: {}", attempt, e),
            }
        }
        if !reachable {
            return Err(Error::disconnected(
                "IDN server did not respond to ping at connect",
            ));
        }

        // Fresh session: reset the estimator so stale decay state from a prior
        // connection does not bias admission.
        self.estimator.reset(Instant::now());

        let (tx, rx) = mpsc::sync_channel(IDN_WORKER_QUEUE_CAPACITY);
        let connected = Arc::new(AtomicBool::new(true));
        let worker_connected = Arc::clone(&connected);
        let point_format = self.point_format;
        let handle = thread::spawn(move || worker_loop(stream, rx, worker_connected, point_format));

        self.runtime = Some(WorkerRuntime {
            tx,
            connected,
            handle: Some(handle),
        });
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(mut runtime) = self.runtime.take() {
            let _ = runtime.tx.send(WorkerCommand::Shutdown);
            if let Some(handle) = runtime.handle.take() {
                let _ = handle.join();
            }
        }
        self.estimator.reset(Instant::now());
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.connected.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(runtime) = &self.runtime {
            let _ = runtime.tx.send(WorkerCommand::Stop);
        }
        self.estimator.reset(Instant::now());
        Ok(())
    }

    fn set_shutter(&mut self, _open: bool) -> Result<()> {
        Ok(())
    }
}

impl FifoBackend for IdnBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        if !runtime.connected.load(Ordering::Acquire) {
            return Err(Error::disconnected("IDN sender thread disconnected"));
        }

        let n = points.len();
        let chunk_points = match self.point_format {
            PointFormat::Xyrgbi => {
                self.point_buffer.clear();
                self.point_buffer
                    .extend(points.iter().map(PointXyrgbi::from));
                ChunkPoints::Xyrgbi(std::mem::take(&mut self.point_buffer))
            }
            PointFormat::XyrgbHighRes => {
                ChunkPoints::HighRes(points.iter().map(PointXyrgbHighRes::from).collect())
            }
            PointFormat::Extended => {
                ChunkPoints::Extended(points.iter().map(PointExtended::from).collect())
            }
        };
        let chunk = QueuedChunk {
            pps,
            points: chunk_points,
        };

        match runtime.tx.try_send(WorkerCommand::Chunk(chunk)) {
            Ok(()) => {
                self.estimator.record_send(Instant::now(), n as u64, pps);
                Ok(WriteOutcome::Written)
            }
            Err(mpsc::TrySendError::Full(_)) => {
                log::debug!("idn worker queue full, back-pressuring scheduler");
                Ok(WriteOutcome::WouldBlock)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(Error::disconnected("IDN sender thread disconnected"))
            }
        }
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

fn blank_chunk(pps: u32, format: PointFormat) -> QueuedChunk {
    let points = match format {
        PointFormat::Xyrgbi => ChunkPoints::Xyrgbi(vec![PointXyrgbi::default(); 10]),
        PointFormat::XyrgbHighRes => ChunkPoints::HighRes(vec![PointXyrgbHighRes::default(); 10]),
        PointFormat::Extended => ChunkPoints::Extended(vec![PointExtended::default(); 10]),
    };
    QueuedChunk { pps, points }
}

fn worker_loop<S: WorkerStream>(
    mut stream: S,
    rx: mpsc::Receiver<WorkerCommand>,
    connected: Arc<AtomicBool>,
    point_format: PointFormat,
) {
    let mut queue = VecDeque::new();
    let mut last_pps = stream.scan_speed();

    // Liveness tracking. Data sends are periodically upgraded to ACK requests
    // (and idle intervals send a ping); after `MAX_CONSECUTIVE_ACK_MISSES`
    // consecutive misses the device is treated as dead so the worker exits and
    // the backend is marked disconnected (triggering the driver's reconnect).
    let mut last_ack_check: Option<Instant> = None;
    let mut misses: u32 = 0;

    'worker: loop {
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    if !handle_worker_command(cmd, &mut queue, last_pps, point_format) {
                        break 'worker;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break 'worker,
            }
        }

        if let Some(chunk) = queue.pop_front() {
            let remaining = queue.len();
            log::trace!(
                "idn worker: sending {} pts, {} queued behind",
                chunk.points.len(),
                remaining,
            );
            last_pps = chunk.pps;
            stream.set_scan_speed(chunk.pps);

            let now = Instant::now();
            if liveness_check_due(last_ack_check, misses, now) {
                last_ack_check = Some(now);
                if stream.write_frame_with_ack(&chunk.points, ACK_TIMEOUT) {
                    misses = 0;
                } else {
                    misses += 1;
                    log::debug!(
                        "idn worker: liveness ACK miss {}/{}",
                        misses,
                        MAX_CONSECUTIVE_ACK_MISSES
                    );
                    if misses >= MAX_CONSECUTIVE_ACK_MISSES {
                        log::warn!(
                            "idn worker: {} consecutive ACK misses; marking disconnected",
                            misses
                        );
                        break;
                    }
                }
            } else if !stream.write_frame(&chunk.points) {
                break;
            }
            continue;
        }

        match rx.recv_timeout(stream::KEEPALIVE_INTERVAL) {
            Ok(cmd) => {
                if !handle_worker_command(cmd, &mut queue, last_pps, point_format) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stream.needs_keepalive() && !stream.send_keepalive() {
                    break;
                }
                // Detect a device that dies while idle: ping periodically.
                let now = Instant::now();
                if liveness_check_due(last_ack_check, misses, now) {
                    last_ack_check = Some(now);
                    if stream.ping(ACK_TIMEOUT) {
                        misses = 0;
                    } else {
                        misses += 1;
                        if misses >= MAX_CONSECUTIVE_ACK_MISSES {
                            log::warn!(
                                "idn worker: {} consecutive ping misses while idle; \
                                 marking disconnected",
                                misses
                            );
                            break;
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    connected.store(false, Ordering::Release);
    stream.close();
}

/// Whether a liveness (ACK/ping) check is due: once we start missing we probe
/// on every opportunity so a dead device is detected quickly; otherwise we
/// probe at most once per `ACK_CHECK_INTERVAL`.
fn liveness_check_due(last_ack_check: Option<Instant>, misses: u32, now: Instant) -> bool {
    misses > 0
        || match last_ack_check {
            None => true,
            Some(t) => now.duration_since(t) >= ACK_CHECK_INTERVAL,
        }
}

fn handle_worker_command(
    cmd: WorkerCommand,
    queue: &mut VecDeque<QueuedChunk>,
    last_pps: u32,
    point_format: PointFormat,
) -> bool {
    match cmd {
        WorkerCommand::Chunk(chunk) => {
            queue.push_back(chunk);
            true
        }
        WorkerCommand::Stop => {
            queue.clear();
            queue.push_back(blank_chunk(last_pps, point_format));
            true
        }
        WorkerCommand::Shutdown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        handle_worker_command, worker_loop, ChunkPoints, QueuedChunk, WorkerCommand, WorkerStream,
        MAX_CONSECUTIVE_ACK_MISSES,
    };
    use crate::protocols::idn::dac::stream::PointFormat;
    use crate::protocols::idn::protocol::PointXyrgbi;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    struct FakeStream {
        scan_speed: u32,
        writes: Arc<AtomicUsize>,
        /// Whether ACK/ping probes succeed (false simulates a dead device).
        ack_ok: Arc<AtomicBool>,
    }

    impl FakeStream {
        fn new(writes: Arc<AtomicUsize>) -> Self {
            Self {
                scan_speed: 30_000,
                writes,
                ack_ok: Arc::new(AtomicBool::new(true)),
            }
        }

        fn with_ack(writes: Arc<AtomicUsize>, ack_ok: Arc<AtomicBool>) -> Self {
            Self {
                scan_speed: 30_000,
                writes,
                ack_ok,
            }
        }
    }

    impl WorkerStream for FakeStream {
        fn scan_speed(&self) -> u32 {
            self.scan_speed
        }

        fn set_scan_speed(&mut self, pps: u32) {
            self.scan_speed = pps;
        }

        fn needs_keepalive(&self) -> bool {
            false
        }

        fn send_keepalive(&mut self) -> bool {
            true
        }

        fn write_frame(&mut self, _points: &ChunkPoints) -> bool {
            self.writes.fetch_add(1, Ordering::Acquire);
            true
        }

        fn write_frame_with_ack(&mut self, _points: &ChunkPoints, _timeout: Duration) -> bool {
            self.writes.fetch_add(1, Ordering::Acquire);
            self.ack_ok.load(Ordering::Acquire)
        }

        fn ping(&mut self, _timeout: Duration) -> bool {
            self.ack_ok.load(Ordering::Acquire)
        }

        fn close(&mut self) {}
    }

    fn test_chunk(pps: u32, point_count: usize) -> QueuedChunk {
        QueuedChunk {
            pps,
            points: ChunkPoints::Xyrgbi(vec![PointXyrgbi::new(0, 0, 0, 0, 0, 0); point_count]),
        }
    }

    fn wait_for_writes(writes: &AtomicUsize, target: usize, timeout: Duration) -> usize {
        let start = Instant::now();
        loop {
            let count = writes.load(Ordering::Acquire);
            if count >= target || start.elapsed() >= timeout {
                return count;
            }
            thread::yield_now();
        }
    }

    #[test]
    fn handle_worker_command_stop_replaces_backlog_with_blank() {
        let mut queue = VecDeque::new();

        assert!(handle_worker_command(
            WorkerCommand::Chunk(test_chunk(30_000, 179)),
            &mut queue,
            30_000,
            PointFormat::Xyrgbi,
        ));
        assert!(handle_worker_command(
            WorkerCommand::Chunk(test_chunk(30_000, 179)),
            &mut queue,
            30_000,
            PointFormat::Xyrgbi,
        ));

        assert!(handle_worker_command(
            WorkerCommand::Stop,
            &mut queue,
            30_000,
            PointFormat::Xyrgbi,
        ));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().points.len(), 10);
    }

    #[test]
    fn worker_loop_bursts_backlog_without_realtime_delay() {
        let writes = Arc::new(AtomicUsize::new(0));
        let fake_stream = FakeStream::new(Arc::clone(&writes));
        let (tx, rx) = mpsc::sync_channel(4);
        let connected = Arc::new(AtomicBool::new(true));
        let worker_connected = Arc::clone(&connected);

        let handle = thread::spawn(move || {
            worker_loop(fake_stream, rx, worker_connected, PointFormat::Xyrgbi)
        });

        tx.send(WorkerCommand::Chunk(test_chunk(1_000, 179)))
            .unwrap();
        tx.send(WorkerCommand::Chunk(test_chunk(1_000, 179)))
            .unwrap();

        let count = wait_for_writes(&writes, 2, Duration::from_millis(20));

        tx.send(WorkerCommand::Shutdown).unwrap();
        handle.join().unwrap();

        assert_eq!(
            count, 2,
            "worker should drain backlog immediately to build timestamp lead"
        );
        assert!(!connected.load(Ordering::Acquire));
    }

    #[test]
    fn worker_disconnects_after_consecutive_ack_misses() {
        let writes = Arc::new(AtomicUsize::new(0));
        // A dead device: ACK/ping probes never succeed.
        let ack_ok = Arc::new(AtomicBool::new(false));
        let fake_stream = FakeStream::with_ack(Arc::clone(&writes), Arc::clone(&ack_ok));
        let (tx, rx) = mpsc::sync_channel(8);
        let connected = Arc::new(AtomicBool::new(true));
        let worker_connected = Arc::clone(&connected);

        let handle = thread::spawn(move || {
            worker_loop(fake_stream, rx, worker_connected, PointFormat::Xyrgbi)
        });

        // Feed more chunks than the miss threshold; the first send is always an
        // ACK request (last_ack_check == None), and once misses start every
        // subsequent send is probed, so the worker gives up quickly.
        for _ in 0..10 {
            let _ = tx.send(WorkerCommand::Chunk(test_chunk(30_000, 20)));
        }

        handle.join().unwrap();

        assert!(
            !connected.load(Ordering::Acquire),
            "worker should mark disconnected after consecutive ACK misses"
        );
        assert!(
            writes.load(Ordering::Acquire) >= MAX_CONSECUTIVE_ACK_MISSES as usize,
            "worker should have attempted the frames before giving up"
        );
    }
}
