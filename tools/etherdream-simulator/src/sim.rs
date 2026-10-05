//! Owns the running simulator and collects the points it plays.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use laser_dac::protocols::ether_dream::sim::{SimServer, SimServerConfig};
use laser_dac::protocols::ether_dream::{DacPoint, FirmwareProfile};

/// Most recent played points kept for rendering.
const HISTORY: usize = 100_000;

#[derive(Clone)]
pub struct SimOptions {
    pub bind: SocketAddr,
    pub broadcast: Option<(SocketAddr, Duration)>,
}

/// A running server plus a thread that drains its played points, so memory
/// stays bounded even while the window is hidden and not repainting.
pub struct SimHandle {
    server: Arc<SimServer>,
    history: Arc<Mutex<VecDeque<DacPoint>>>,
    stop: Arc<AtomicBool>,
    drain: Option<JoinHandle<()>>,
}

impl SimHandle {
    pub fn start(profile: FirmwareProfile, options: SimOptions) -> io::Result<Self> {
        let mut config = SimServerConfig::new(profile).with_tcp_addr(options.bind);
        // The GUI never reads the input log; keep memory bounded.
        config.record_input = false;
        if let Some((target, interval)) = options.broadcast {
            config = config.with_broadcast(target, interval);
        }
        let server = Arc::new(SimServer::start(config)?);
        server.with_model(|m| m.set_record_output(true));

        let history = Arc::new(Mutex::new(VecDeque::with_capacity(HISTORY)));
        let stop = Arc::new(AtomicBool::new(false));
        let drain = {
            let (server, history, stop) = (server.clone(), history.clone(), stop.clone());
            thread::Builder::new()
                .name("etherdream-sim-drain".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let played = server.with_model(|m| m.take_output());
                        if !played.is_empty() {
                            let mut h = history.lock().unwrap_or_else(|e| e.into_inner());
                            h.extend(played);
                            let excess = h.len().saturating_sub(HISTORY);
                            h.drain(..excess);
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                })?
        };
        Ok(Self {
            server,
            history,
            stop,
            drain: Some(drain),
        })
    }

    pub fn server(&self) -> &SimServer {
        &self.server
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    /// The last `n` points played, oldest first.
    pub fn recent(&self, n: usize) -> Vec<DacPoint> {
        let h = self.history.lock().unwrap_or_else(|e| e.into_inner());
        let skip = h.len().saturating_sub(n);
        h.iter().skip(skip).copied().collect()
    }

    pub fn clear_history(&self) {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

impl Drop for SimHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.drain.take() {
            let _ = t.join();
        }
        // The server, and with it the listening sockets, drops after the
        // drain thread released its reference.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use laser_dac::protocols::ether_dream::sim::model::cmd;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Instant;

    fn loopback() -> SimOptions {
        SimOptions {
            bind: "127.0.0.1:0".parse().unwrap(),
            broadcast: None,
        }
    }

    #[test]
    fn played_points_reach_the_history() {
        let sim = SimHandle::start(FirmwareProfile::ed2_r331(), loopback()).unwrap();
        let mut s = TcpStream::connect(sim.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut reply = [0u8; 22];
        s.read_exact(&mut reply).unwrap(); // hello
        for bytes in [b"p".to_vec(), cmd::blank_data(300), cmd::begin(30_000)] {
            s.write_all(&bytes).unwrap();
            s.read_exact(&mut reply).unwrap();
            assert_eq!(reply[0], b'a', "command {:?} not ACKed", bytes[0] as char);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while sim.recent(1000).len() < 300 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sim.recent(1000).len(), 300);
        sim.clear_history();
        assert!(sim.recent(1000).is_empty());
    }

    #[test]
    fn restarting_on_the_same_port_works() {
        let first = SimHandle::start(FirmwareProfile::ed2_r331(), loopback()).unwrap();
        let options = SimOptions {
            bind: first.addr(),
            broadcast: None,
        };
        drop(first);
        let second = SimHandle::start(FirmwareProfile::ed1_j4cdac(), options.clone()).unwrap();
        assert_eq!(second.addr(), options.bind);
    }
}
