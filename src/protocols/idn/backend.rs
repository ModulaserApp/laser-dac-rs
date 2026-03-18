//! IDN DAC streaming backend implementation.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::idn::dac::{stream, ServerInfo, ServiceInfo};
use crate::protocols::idn::protocol::PointXyrgbi;
use crate::types::{DacCapabilities, DacType, LaserPoint};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

const IDN_WORKER_QUEUE_CAPACITY: usize = 8;

struct WorkerRuntime {
    tx: mpsc::SyncSender<WorkerCommand>,
    connected: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

struct QueuedChunk {
    pps: u32,
    points: Vec<PointXyrgbi>,
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
    fn write_frame(&mut self, points: &[PointXyrgbi]) -> bool;
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

    fn write_frame(&mut self, points: &[PointXyrgbi]) -> bool {
        stream::Stream::write_frame(self, points).is_ok()
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
    /// Conversion buffer moved into each chunk (capacity reused across writes).
    point_buffer: Vec<PointXyrgbi>,
}

impl IdnBackend {
    pub fn new(server: ServerInfo, service: ServiceInfo) -> Self {
        Self {
            server,
            service,
            runtime: None,
            caps: super::default_capabilities(),
            point_buffer: Vec::new(),
        }
    }
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
        let (tx, rx) = mpsc::sync_channel(IDN_WORKER_QUEUE_CAPACITY);
        let connected = Arc::new(AtomicBool::new(true));
        let worker_connected = Arc::clone(&connected);
        let handle = thread::spawn(move || worker_loop(stream, rx, worker_connected));

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

        self.point_buffer.clear();
        self.point_buffer
            .extend(points.iter().map(PointXyrgbi::from));

        let chunk = QueuedChunk {
            pps,
            points: std::mem::take(&mut self.point_buffer),
        };

        match runtime.tx.try_send(WorkerCommand::Chunk(chunk)) {
            Ok(()) => Ok(WriteOutcome::Written),
            Err(mpsc::TrySendError::Full(_)) => {
                log::debug!("idn worker queue full, back-pressuring scheduler");
                Ok(WriteOutcome::WouldBlock)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                Err(Error::disconnected("IDN sender thread disconnected"))
            }
        }
    }
}

fn blank_chunk(pps: u32) -> QueuedChunk {
    let points = vec![PointXyrgbi::new(0, 0, 0, 0, 0, 0); 10];
    QueuedChunk { pps, points }
}

fn worker_loop<S: WorkerStream>(
    mut stream: S,
    rx: mpsc::Receiver<WorkerCommand>,
    connected: Arc<AtomicBool>,
) {
    let mut queue = VecDeque::new();
    let mut last_pps = stream.scan_speed();

    'worker: loop {
        loop {
            match rx.try_recv() {
                Ok(cmd) => {
                    if !handle_worker_command(cmd, &mut queue, last_pps) {
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
            if !stream.write_frame(&chunk.points) {
                break;
            }
            continue;
        }

        match rx.recv_timeout(stream::KEEPALIVE_INTERVAL) {
            Ok(cmd) => {
                if !handle_worker_command(cmd, &mut queue, last_pps) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stream.needs_keepalive() && !stream.send_keepalive() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    connected.store(false, Ordering::Release);
    stream.close();
}

fn handle_worker_command(
    cmd: WorkerCommand,
    queue: &mut VecDeque<QueuedChunk>,
    last_pps: u32,
) -> bool {
    match cmd {
        WorkerCommand::Chunk(chunk) => {
            queue.push_back(chunk);
            true
        }
        WorkerCommand::Stop => {
            queue.clear();
            queue.push_back(blank_chunk(last_pps));
            true
        }
        WorkerCommand::Shutdown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{handle_worker_command, worker_loop, QueuedChunk, WorkerCommand, WorkerStream};
    use crate::protocols::idn::protocol::PointXyrgbi;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread;
    use std::time::{Duration, Instant};

    struct FakeStream {
        scan_speed: u32,
        writes: Arc<AtomicUsize>,
    }

    impl FakeStream {
        fn new(writes: Arc<AtomicUsize>) -> Self {
            Self {
                scan_speed: 30_000,
                writes,
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

        fn write_frame(&mut self, _points: &[PointXyrgbi]) -> bool {
            self.writes.fetch_add(1, Ordering::Acquire);
            true
        }

        fn close(&mut self) {}
    }

    fn test_chunk(pps: u32, point_count: usize) -> QueuedChunk {
        QueuedChunk {
            pps,
            points: vec![PointXyrgbi::new(0, 0, 0, 0, 0, 0); point_count],
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
        ));
        assert!(handle_worker_command(
            WorkerCommand::Chunk(test_chunk(30_000, 179)),
            &mut queue,
            30_000,
        ));

        assert!(handle_worker_command(
            WorkerCommand::Stop,
            &mut queue,
            30_000
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

        let handle = thread::spawn(move || worker_loop(fake_stream, rx, worker_connected));

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
}
