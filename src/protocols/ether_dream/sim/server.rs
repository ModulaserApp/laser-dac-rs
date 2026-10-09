//! TCP command server and UDP broadcaster around an [`EtherDreamModel`].
//!
//! Tests bind to `127.0.0.1:0` and connect by address
//! ([`crate::protocols::ether_dream::dac::stream::connect_to`]). The simulator
//! tool binds the real ports, 7765 for TCP and broadcasts to UDP 7654.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::model::{command_complete, EtherDreamModel, Reply};
use crate::protocols::ether_dream::profile::FirmwareProfile;
use crate::protocols::ether_dream::protocol::{DacBroadcast, DacStatus, WriteBytes};

/// Network faults the server can inject. All default to off.
///
/// Reply indices count the replies sent on one connection after the hello,
/// starting at 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Faults {
    /// Close the connection instead of sending reply number N.
    pub drop_after_replies: Option<usize>,
    /// Send only the first 2 bytes of reply number N, then close.
    pub truncate_reply: Option<usize>,
    /// Wait this long before sending each reply.
    pub reply_delay: Duration,
    /// Send every normal response twice.
    pub duplicate_replies: bool,
    /// Process commands with these opcodes but never answer them.
    pub swallow_opcodes: Vec<u8>,
    /// Replace the status in the connection hello, for example with an
    /// invalid light-engine state.
    pub hello_status: Option<DacStatus>,
    /// Handle each `'d'` this long after it arrives, as if its payload were
    /// still on the wire. Earlier commands in the same packet are answered
    /// first and the DAC keeps playing meanwhile.
    pub data_delay: Duration,
}

/// Where and how often to send UDP broadcasts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BroadcastConfig {
    /// Destination, e.g. `255.255.255.255:7654` or `127.0.0.1:7654`.
    pub target: SocketAddr,
    /// Interval between broadcasts. Real hardware sends one per second.
    pub interval: Duration,
}

/// Configuration for [`SimServer::start`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SimServerConfig {
    /// Firmware to simulate.
    pub profile: FirmwareProfile,
    /// TCP address to listen on. Defaults to `127.0.0.1:0`.
    pub tcp_addr: SocketAddr,
    /// Send UDP broadcasts. Off by default.
    pub broadcast: Option<BroadcastConfig>,
    /// Refuse a second concurrent TCP client. Defaults to the profile's
    /// [`FirmwareProfile::enforce_single_client`].
    pub enforce_single_client: bool,
    /// Record commands and received points (see
    /// [`EtherDreamModel::set_record_input`]).
    pub record_input: bool,
    /// Initial faults.
    pub faults: Faults,
}

impl SimServerConfig {
    /// Loopback, ephemeral port, no broadcasts, input recording on.
    pub fn new(profile: FirmwareProfile) -> Self {
        Self {
            enforce_single_client: profile.enforce_single_client,
            profile,
            tcp_addr: SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
            broadcast: None,
            record_input: true,
            faults: Faults::default(),
        }
    }

    /// Set the faults.
    pub fn with_faults(mut self, faults: Faults) -> Self {
        self.faults = faults;
        self
    }

    /// Set the TCP listen address.
    pub fn with_tcp_addr(mut self, addr: SocketAddr) -> Self {
        self.tcp_addr = addr;
        self
    }

    /// Enable UDP broadcasts.
    pub fn with_broadcast(mut self, target: SocketAddr, interval: Duration) -> Self {
        let interval = interval.max(Duration::from_millis(1));
        self.broadcast = Some(BroadcastConfig { target, interval });
        self
    }
}

struct Shared {
    model: Mutex<EtherDreamModel>,
    faults: Mutex<Faults>,
    epoch: Instant,
    stop: AtomicBool,
    active: AtomicUsize,
    accepted: AtomicUsize,
    enforce_single_client: bool,
}

impl Shared {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn lock_model(&self) -> MutexGuard<'_, EtherDreamModel> {
        let mut m = self.model.lock().unwrap_or_else(|e| e.into_inner());
        m.advance_to(self.now());
        m
    }

    fn faults(&self) -> Faults {
        self.faults
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// A running simulated DAC. Dropping it stops every thread.
pub struct SimServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl SimServer {
    /// Bind and start serving.
    pub fn start(config: SimServerConfig) -> io::Result<SimServer> {
        let listener = TcpListener::bind(config.tcp_addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;

        let mut model = EtherDreamModel::new(config.profile);
        model.set_record_input(config.record_input);
        let shared = Arc::new(Shared {
            model: Mutex::new(model),
            faults: Mutex::new(config.faults),
            epoch: Instant::now(),
            stop: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            accepted: AtomicUsize::new(0),
            enforce_single_client: config.enforce_single_client,
        });
        let threads = Arc::new(Mutex::new(Vec::new()));
        // Set up the broadcast socket first: an error after the accept thread
        // is running would leave it holding the TCP port forever.
        let broadcast = match config.broadcast {
            Some(bc) => {
                let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
                socket.set_broadcast(true)?;
                Some((socket, bc))
            }
            None => None,
        };
        let server = SimServer {
            shared,
            addr,
            threads,
        };
        // From here on, dropping `server` on an error stops and joins
        // whatever was already spawned.
        let accept = {
            let shared = Arc::clone(&server.shared);
            let threads = Arc::clone(&server.threads);
            thread::Builder::new()
                .name("ether-dream-sim-accept".into())
                .spawn(move || accept_loop(listener, shared, threads))?
        };
        server.threads.lock().unwrap().push(accept);

        if let Some((socket, bc)) = broadcast {
            let shared = Arc::clone(&server.shared);
            let handle = thread::Builder::new()
                .name("ether-dream-sim-broadcast".into())
                .spawn(move || broadcast_loop(socket, bc, shared))?;
            server.threads.lock().unwrap().push(handle);
        }

        Ok(server)
    }

    /// Start a loopback server for `profile` with default settings.
    pub fn loopback(profile: FirmwareProfile) -> io::Result<SimServer> {
        Self::start(SimServerConfig::new(profile))
    }

    /// The bound TCP address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The current broadcast frame (what a discoverer would receive).
    pub fn broadcast(&self) -> DacBroadcast {
        self.shared.lock_model().broadcast()
    }

    /// Time since the server started, the model's clock.
    pub fn now(&self) -> Duration {
        self.shared.now()
    }

    /// Run `f` with the model, advanced to the current time.
    pub fn with_model<R>(&self, f: impl FnOnce(&mut EtherDreamModel) -> R) -> R {
        f(&mut self.shared.lock_model())
    }

    /// Current status.
    pub fn status(&self) -> DacStatus {
        self.with_model(|m| m.status())
    }

    /// Opcodes of every command processed, across all connections.
    pub fn commands(&self) -> Vec<u8> {
        self.with_model(|m| m.commands().to_vec())
    }

    /// Replace the active faults. Applies to replies sent from now on.
    pub fn set_faults(&self, faults: Faults) {
        *self.shared.faults.lock().unwrap_or_else(|e| e.into_inner()) = faults;
    }

    /// The active faults.
    pub fn faults(&self) -> Faults {
        self.shared.faults()
    }

    /// Connections currently open.
    pub fn active_connections(&self) -> usize {
        self.shared.active.load(Ordering::SeqCst)
    }

    /// Connections accepted since start (refused ones excluded).
    pub fn total_connections(&self) -> usize {
        self.shared.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for SimServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        // Connection threads are pushed by the accept thread, so keep joining
        // until the list stays empty.
        loop {
            let batch: Vec<_> = std::mem::take(&mut *self.threads.lock().unwrap());
            if batch.is_empty() {
                break;
            }
            for t in batch {
                let _ = t.join();
            }
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
) {
    while !shared.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((sock, _)) => {
                if shared.enforce_single_client && shared.active.load(Ordering::SeqCst) > 0 {
                    drop(sock);
                    continue;
                }
                shared.active.fetch_add(1, Ordering::SeqCst);
                let conn_shared = Arc::clone(&shared);
                let spawned = thread::Builder::new()
                    .name("ether-dream-sim-conn".into())
                    .spawn(move || {
                        let _ = handle_connection(sock, &conn_shared);
                        // All clients share one playback state, so only the
                        // last connection closing counts as a disconnect.
                        if conn_shared.active.fetch_sub(1, Ordering::SeqCst) == 1 {
                            let now = conn_shared.now();
                            conn_shared.lock_model().connection_closed(now);
                        }
                    });
                match spawned {
                    Ok(h) => {
                        shared.accepted.fetch_add(1, Ordering::SeqCst);
                        let mut threads = threads.lock().unwrap();
                        // Reap finished connections so a long run with many
                        // reconnects does not pile up unjoined threads.
                        for t in extract_finished(&mut threads) {
                            let _ = t.join();
                        }
                        threads.push(h);
                    }
                    Err(e) => {
                        // The socket was dropped with the closure; undo the count.
                        shared.active.fetch_sub(1, Ordering::SeqCst);
                        log::warn!("ether dream sim: spawn failed: {e}");
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(e) => {
                log::warn!("ether dream sim: accept failed: {e}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Remove and return the handles of threads that have exited.
fn extract_finished(threads: &mut Vec<JoinHandle<()>>) -> Vec<JoinHandle<()>> {
    let (done, running) = std::mem::take(threads)
        .into_iter()
        .partition(|t| t.is_finished());
    *threads = running;
    done
}

fn broadcast_loop(socket: UdpSocket, config: BroadcastConfig, shared: Arc<Shared>) {
    let mut next = Instant::now();
    let mut warned = false;
    while !shared.stop.load(Ordering::SeqCst) {
        if Instant::now() >= next {
            let frame = shared.lock_model().broadcast();
            let mut bytes = Vec::new();
            bytes.write_bytes(frame).expect("Vec write cannot fail");
            if let Err(e) = socket.send_to(&bytes, config.target) {
                // Warn once: discovery silently never works otherwise.
                if !warned {
                    warned = true;
                    log::warn!(
                        "ether dream sim: broadcast to {} failed: {e}",
                        config.target
                    );
                } else {
                    log::debug!("ether dream sim: broadcast send failed: {e}");
                }
            }
            // Skip missed slots after a stall instead of bursting to catch up.
            next = (next + config.interval).max(Instant::now());
        }
        thread::sleep(Duration::from_millis(2).min(config.interval));
    }
}

fn reset(sock: &TcpStream) {
    // A zero linger makes close() send RST, like the firmware does.
    let _ = socket2::SockRef::from(sock).set_linger(Some(Duration::ZERO));
}

/// Sleep for `d`, waking early when the server is stopping. Returns `false`
/// if it stopped.
fn sleep_unless_stopped(shared: &Shared, d: Duration) -> bool {
    let deadline = Instant::now() + d;
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
}

fn handle_connection(mut sock: TcpStream, shared: &Shared) -> io::Result<()> {
    // On macOS and the BSDs an accepted socket inherits the listener's
    // O_NONBLOCK, which would turn the read timeout into a busy loop.
    sock.set_nonblocking(false)?;
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(5)))?;
    // A client that stops reading must not wedge the thread, or `Drop`.
    sock.set_write_timeout(Some(Duration::from_secs(1)))?;

    let mut hung = {
        let model = shared.lock_model();
        if model.is_hung() {
            true
        } else {
            let mut hello = model.hello();
            drop(model);
            if let Some(st) = shared.faults().hello_status {
                hello.dac_status = st;
            }
            sock.write_all(&Reply::Response(hello).to_bytes())?;
            false
        }
    };

    let mut input = Vec::new();
    let mut buf = [0u8; 8192];
    let mut sent = 0usize;
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let n = match sock.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e),
        };
        if hung {
            continue;
        }
        input.extend_from_slice(&buf[..n]);
        let faults = shared.faults();

        loop {
            let (replies, data_ready) = {
                let mut model = shared.lock_model();
                let now = shared.now();
                if faults.data_delay.is_zero() {
                    (model.process(now, &mut input), false)
                } else {
                    let replies = model.process_before(now, &mut input, b'd');
                    (
                        replies,
                        input.first() == Some(&b'd') && command_complete(&input),
                    )
                }
            };
            if !send_replies(&sock, shared, replies, &faults, &mut sent, &mut hung)? {
                return Ok(());
            }
            if hung || !data_ready {
                break;
            }
            // The 'd' payload is still "in flight": the DAC keeps playing
            // while it arrives and only then handles the command.
            if !sleep_unless_stopped(shared, faults.data_delay) {
                return Ok(());
            }
            let replies = {
                let mut model = shared.lock_model();
                let now = shared.now();
                model.process_one(now, &mut input)
            };
            if !send_replies(&sock, shared, replies, &faults, &mut sent, &mut hung)? {
                return Ok(());
            }
            if hung {
                break;
            }
        }
    }
}

/// Send `replies`, applying the reply faults. Returns `Ok(false)` when the
/// connection must close.
fn send_replies(
    mut sock: &TcpStream,
    shared: &Shared,
    replies: Vec<Reply>,
    faults: &Faults,
    sent: &mut usize,
    hung: &mut bool,
) -> io::Result<bool> {
    for reply in replies {
        match reply {
            Reply::Reset => {
                reset(sock);
                return Ok(false);
            }
            Reply::Hang => {
                *hung = true;
                break;
            }
            Reply::Response(ref r) if faults.swallow_opcodes.contains(&r.command) => continue,
            _ => {}
        }
        if faults.drop_after_replies == Some(*sent) {
            return Ok(false);
        }
        if !sleep_unless_stopped(shared, faults.reply_delay) {
            return Ok(false);
        }
        let bytes = reply.to_bytes();
        if faults.truncate_reply == Some(*sent) {
            sock.write_all(&bytes[..2.min(bytes.len())])?;
            return Ok(false);
        }
        sock.write_all(&bytes)?;
        if faults.duplicate_replies && matches!(reply, Reply::Response(_)) {
            sock.write_all(&bytes)?;
        }
        *sent += 1;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::super::model::cmd;
    use super::*;
    use crate::protocols::ether_dream::protocol::{DacResponse, ReadBytes, SizeBytes};

    fn read_resp(s: &mut TcpStream) -> DacResponse {
        let mut b = [0u8; DacResponse::SIZE_BYTES];
        s.read_exact(&mut b).unwrap();
        (&b[..]).read_bytes::<DacResponse>().unwrap()
    }

    fn connect(srv: &SimServer) -> TcpStream {
        let s = TcpStream::connect(srv.addr()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        s
    }

    #[test]
    fn data_delay_answers_earlier_commands_first_and_delays_the_data() {
        let f = Faults {
            data_delay: Duration::from_millis(30),
            ..Faults::default()
        };
        let srv =
            SimServer::start(SimServerConfig::new(FirmwareProfile::ed2_r331()).with_faults(f))
                .unwrap();
        let mut sock = connect(&srv);
        let mut hello = [0u8; 22];
        sock.read_exact(&mut hello).unwrap();

        // Only lower bounds on elapsed time, so a slow runner cannot fail it.
        let start = Instant::now();
        let bytes = [cmd::prepare(), cmd::blank_data(10), b"?".to_vec()].concat();
        sock.write_all(&bytes).unwrap();
        let mut reply = [0u8; 22];
        sock.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], b'p');
        sock.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], b'd');
        assert!(start.elapsed() >= Duration::from_millis(30), "'d' delayed");
        sock.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], b'?');
        assert_eq!(srv.status().buffer_fullness, 10);
    }

    #[test]
    fn hello_then_ping() {
        let srv = SimServer::loopback(FirmwareProfile::ed2_r331()).unwrap();
        let mut s = connect(&srv);
        assert_eq!(read_resp(&mut s).command, b'?');
        s.write_all(b"?").unwrap();
        assert_eq!(read_resp(&mut s).response, DacResponse::ACK);
        assert_eq!(srv.commands(), b"?");
    }

    #[test]
    fn unknown_command_resets_connection() {
        let srv = SimServer::loopback(FirmwareProfile::ed2_r331()).unwrap();
        let mut s = connect(&srv);
        read_resp(&mut s);
        s.write_all(b"z").unwrap();
        let mut b = [0u8; 1];
        match s.read(&mut b) {
            Ok(0) => {}
            Err(e) => assert!(
                matches!(
                    e.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                ),
                "{e:?}"
            ),
            Ok(n) => panic!("unexpected {n} bytes"),
        }
    }

    #[test]
    fn duplicate_and_swallow_faults() {
        let faults = Faults {
            duplicate_replies: true,
            swallow_opcodes: vec![b'p'],
            ..Faults::default()
        };
        let srv =
            SimServer::start(SimServerConfig::new(FirmwareProfile::ed2_r331()).with_faults(faults))
                .unwrap();
        let mut s = connect(&srv);
        read_resp(&mut s);
        s.write_all(&cmd::prepare()).unwrap();
        s.write_all(b"?").unwrap();
        assert_eq!(read_resp(&mut s).command, b'?', "prepare was swallowed");
        assert_eq!(read_resp(&mut s).command, b'?', "duplicated");
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PREPARED);
    }

    #[test]
    fn single_client_enforced_when_configured() {
        let mut cfg = SimServerConfig::new(FirmwareProfile::ed2_r331());
        cfg.enforce_single_client = true;
        let srv = SimServer::start(cfg).unwrap();
        let mut a = connect(&srv);
        read_resp(&mut a);
        let mut b = connect(&srv);
        let mut buf = [0u8; 1];
        assert!(matches!(b.read(&mut buf), Ok(0) | Err(_)));
        assert_eq!(srv.total_connections(), 1);
    }

    #[test]
    fn playback_state_survives_reconnect() {
        let srv = SimServer::loopback(FirmwareProfile::ed2_r331()).unwrap();
        {
            let mut s = connect(&srv);
            read_resp(&mut s);
            s.write_all(&cmd::prepare()).unwrap();
            read_resp(&mut s);
        }
        let mut s = connect(&srv);
        let hello = read_resp(&mut s);
        assert_eq!(
            hello.dac_status.playback_state,
            DacStatus::PLAYBACK_PREPARED
        );
    }

    #[test]
    fn closing_tcp_while_playing_stops_playback() {
        // ED2 r331 hardware: after the client closed TCP mid-stream, the next
        // hello showed Idle with the leftover fullness and no underflow flag.
        let srv = SimServer::loopback(FirmwareProfile::ed2_r331()).unwrap();
        {
            let mut s = connect(&srv);
            read_resp(&mut s);
            s.write_all(&cmd::prepare()).unwrap();
            read_resp(&mut s);
            s.write_all(&cmd::blank_data(1_000)).unwrap();
            read_resp(&mut s);
            s.write_all(&cmd::begin(1_000)).unwrap();
            assert_eq!(
                read_resp(&mut s).dac_status.playback_state,
                DacStatus::PLAYBACK_PLAYING
            );
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while srv.active_connections() > 0 {
            assert!(std::time::Instant::now() < deadline, "close not seen");
            thread::sleep(Duration::from_millis(2));
        }
        let mut s = connect(&srv);
        let hello = read_resp(&mut s).dac_status;
        assert_eq!(hello.playback_state, DacStatus::PLAYBACK_IDLE);
        assert!(hello.buffer_fullness > 0, "leftover fullness frozen");
        assert_eq!(hello.playback_flags & 0x2, 0, "no underflow flag");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(srv.status().buffer_fullness, hello.buffer_fullness);
        assert_eq!(srv.with_model(|m| m.underflows()), 0);
    }

    #[test]
    fn broadcasts_reach_target() {
        let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let cfg = SimServerConfig::new(FirmwareProfile::ed1_j4cdac())
            .with_broadcast(rx.local_addr().unwrap(), Duration::from_millis(10));
        let _srv = SimServer::start(cfg).unwrap();
        let mut b = [0u8; 64];
        let (n, _) = rx.recv_from(&mut b).unwrap();
        let bc = (&b[..n]).read_bytes::<DacBroadcast>().unwrap();
        assert_eq!(bc.buffer_capacity, 1799);
    }
}
