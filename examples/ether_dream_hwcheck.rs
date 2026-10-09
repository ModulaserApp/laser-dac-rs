//! Check the Ether Dream backend against a real DAC (blanked points only).
//!
//! Runs named scenarios that drive the production code path:
//! `EtherDreamBackend`, either directly or inside the presentation layer's
//! `Stream`. The backend talks to a local TCP proxy that forwards to the DAC.
//! The proxy records every command and reply in the JSONL format of
//! `examples/ether_dream_capture.rs` (see `tests/fixtures/ether_dream/README.md`),
//! so verdicts come from the bytes on the wire and the status the DAC
//! reported, not from trusting the code.
//!
//! SAFETY. The proxy is also an interlock. It refuses to forward:
//!
//! * a data point with any colour, intensity, control or user bit set, or with
//!   x or y further than 1 count from the centre,
//! * a `'b'`, `'u'` or `'q'` rate outside 1000..=30000, or a nonzero low-water
//!   mark,
//! * an unknown opcode, or a half-written command,
//! * anything but `'?'` and `'c'` while it knows the DAC is in e-stop. Those
//!   commands get a synthesized NAK-Invalid carrying the last real status,
//!   which is what j4cDAC firmware answers. Data during e-stop is untested on
//!   ED2 hardware, so it never reaches the DAC.
//!
//! When it refuses a command it stops the DAC on the same connection, closes
//! it, and fails the scenario (except the one block `clamp_guard` expects).
//! Before every upstream connection it runs `lsof -nP -iTCP:7765` and aborts
//! if another process holds a connection or `lsof` cannot run, and it aborts
//! unless the DAC's hello shows playback idle. It refuses to start if the DAC
//! advertises a maximum rate below the top of the forwarded range. The example never arms the stream, and
//! its backend wrapper overwrites every point with a blanked centre point
//! before the backend sees it.
//!
//! ```text
//! cargo run --example ether_dream_hwcheck --features testutils,ether-dream -- \
//!     --addr 192.168.254.66:7765 --out /tmp/ed-hwcheck
//! cargo run --example ether_dream_hwcheck --features testutils,ether-dream -- \
//!     --sim ed2-r331 --out /tmp/ed-hwcheck-sim --steady-secs 3
//! ```

use std::fs::File;
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use laser_dac::buffer_estimate::BufferEstimator;
use laser_dac::protocols::ether_dream::dac::stream;
use laser_dac::protocols::ether_dream::protocol::{
    DacBroadcast, DacResponse, DacStatus, ReadBytes, SizeBytes, WriteBytes,
};
use laser_dac::protocols::ether_dream::sim::model::cmd;
use laser_dac::protocols::ether_dream::sim::SimServer;
use laser_dac::protocols::ether_dream::{recv_dac_broadcasts, EtherDreamBackend, FirmwareProfile};
use laser_dac::{
    BackendKind, ChunkRequest, ChunkResult, Dac, DacBackend, DacCapabilities, DacDiscovery,
    DacInfo, DacType, EnabledDacTypes, FifoBackend, LaserPoint, SessionControl, StreamConfig,
    WriteOutcome,
};
use serde_json::{json, Value};

/// The only rates the proxy forwards.
const SAFE_RATES: std::ops::RangeInclusive<u32> = 1_000..=30_000;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);
const PBF_UNDERFLOW: u16 = 0x2;
const PBF_ESTOP: u16 = 0x4;

#[derive(Parser)]
#[command(about = "Validate the Ether Dream backend on a real DAC (blanked points only)")]
struct Args {
    /// DAC TCP address, e.g. 192.168.254.66:7765.
    #[arg(long, conflicts_with = "sim")]
    addr: Option<SocketAddr>,
    /// Run against the simulator with this firmware profile instead.
    #[arg(long)]
    sim: Option<String>,
    /// Output directory for traces and the summary.
    #[arg(long, default_value = ".")]
    out: PathBuf,
    /// File name prefix; traces are `<prefix>_<scenario>.jsonl`.
    #[arg(long, default_value = "ed2_r331_hwcheck")]
    prefix: String,
    /// Comma-separated scenario names. Default: all, in order.
    #[arg(long, value_delimiter = ',')]
    scenarios: Vec<String>,
    /// Duration of the steady_stream scenario.
    #[arg(long, default_value_t = 60)]
    steady_secs: u64,
    /// Point rate of the steady_stream scenario (the proxy still caps it).
    #[arg(long, default_value_t = 30_000)]
    steady_pps: u32,
    /// Target buffer in ms for the steady_stream scenario. Default: the
    /// backend's default for Ether Dream.
    #[arg(long)]
    steady_target_ms: Option<u64>,
}

const ALL_SCENARIOS: [&str; 10] = [
    "broadcast_discovery",
    "stale_full_reconnect",
    "underflow_recovery",
    "estop_recovery",
    "rate_change",
    "full_capacity_chunk",
    "clamp_guard",
    "steady_stream_60s",
    "partial_room",
    "session_stop",
];

// --- Trace recording ------------------------------------------------------------

/// Trace lines are kept in memory and written when the scenario ends, so disk
/// I/O never sits between a DAC reply and its delivery to the backend.
struct Recorder {
    path: PathBuf,
    lines: Vec<String>,
    phase: String,
    seq: u32,
    t0: Instant,
    last: Duration,
}

impl Recorder {
    fn new(path: &Path, phase: &str) -> io::Result<Self> {
        File::create(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            lines: Vec::new(),
            phase: phase.to_string(),
            seq: 0,
            t0: Instant::now(),
            last: Duration::ZERO,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        &mut self,
        op: &str,
        req: &[u8],
        n: usize,
        resp: &[u8],
        status: &str,
        start: Instant,
        end: Instant,
        note: &str,
    ) {
        self.seq += 1;
        let t = start.saturating_duration_since(self.t0);
        let end = end.saturating_duration_since(self.t0);
        let line = json!({
            "seq": self.seq,
            "phase": self.phase,
            "op": op,
            "req": hex(req),
            "resp": hex(resp),
            "status": status,
            "n": n,
            "dt_us": t.saturating_sub(self.last).as_micros() as u64,
            "dur_us": end.saturating_sub(t).as_micros() as u64,
            "t_us": end.as_micros() as u64,
            "note": note,
        });
        self.last = end;
        self.lines.push(line.to_string());
    }

    fn save(&self) -> io::Result<()> {
        let mut out = BufWriter::new(File::create(&self.path)?);
        for l in &self.lines {
            writeln!(out, "{l}")?;
        }
        out.flush()
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn op_name(op: u8) -> &'static str {
    match op {
        b'?' => "ping",
        b'p' => "prepare",
        b's' => "stop",
        b'c' => "clear_estop",
        b'v' => "version",
        b'b' => "begin",
        b'u' => "update",
        b'q' => "point_rate",
        b'd' => "data",
        0x00 | 0xff => "estop",
        _ => "unknown",
    }
}

/// Length of the command at the start of `buf`: `None` if more bytes are
/// needed to tell, `Err(op)` for an unknown opcode.
fn cmd_len(buf: &[u8]) -> Option<Result<usize, u8>> {
    let op = *buf.first()?;
    Some(match op {
        b'?' | b'p' | b's' | b'c' | b'v' | 0x00 | 0xff => Ok(1),
        b'b' | b'u' => Ok(7),
        b'q' => Ok(5),
        b'd' => {
            if buf.len() < 3 {
                return None;
            }
            Ok(3 + u16::from_le_bytes([buf[1], buf[2]]) as usize * 18)
        }
        other => Err(other),
    })
}

fn rate_of(cmd: &[u8]) -> Option<u32> {
    match cmd[0] {
        b'b' | b'u' if cmd.len() >= 7 => Some(u32::from_le_bytes([cmd[3], cmd[4], cmd[5], cmd[6]])),
        b'q' if cmd.len() >= 5 => Some(u32::from_le_bytes([cmd[1], cmd[2], cmd[3], cmd[4]])),
        _ => None,
    }
}

/// Why the proxy refuses to forward `cmd`, if it does.
fn unsafe_reason(cmd: &[u8]) -> Option<String> {
    match cmd[0] {
        b'b' | b'u' | b'q' => {
            let rate = rate_of(cmd)?;
            if !SAFE_RATES.contains(&rate) {
                return Some(format!("rate {rate} outside {SAFE_RATES:?}"));
            }
            if cmd[0] != b'q' && (cmd[1] != 0 || cmd[2] != 0) {
                return Some("nonzero low-water mark".into());
            }
            None
        }
        b'd' => {
            for (i, p) in cmd[3..].chunks_exact(18).enumerate() {
                let w = |k: usize| u16::from_le_bytes([p[2 * k], p[2 * k + 1]]);
                let lit = w(0) != 0 || (3..9).any(|k| w(k) != 0);
                let off = |k: usize| (w(k) as i16).unsigned_abs() > 1;
                if lit || off(1) || off(2) {
                    return Some(format!(
                        "point {i} is not a blanked centre point: {}",
                        hex(p)
                    ));
                }
            }
            None
        }
        _ => None,
    }
}

fn parse_resp(resp: &[u8]) -> Option<DacResponse> {
    if resp.len() != DacResponse::SIZE_BYTES {
        return None;
    }
    (&resp[..]).read_bytes::<DacResponse>().ok()
}

fn resp_bytes(r: DacResponse) -> Vec<u8> {
    let mut v = Vec::with_capacity(DacResponse::SIZE_BYTES);
    v.write_bytes(r).expect("vec write");
    v
}

/// Commands that bring a DAC in `st` back to idle using only sequences
/// already probed on ED2 hardware (stop while playing).
fn stop_cmds(st: &DacStatus) -> Vec<Vec<u8>> {
    match st.playback_state {
        DacStatus::PLAYBACK_PLAYING => vec![b"s".to_vec()],
        DacStatus::PLAYBACK_PREPARED => vec![cmd::begin(1_000), b"s".to_vec()],
        _ => vec![],
    }
}

// --- Proxy ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// From the client, reply forwarded back.
    Forward,
    /// Injected by the proxy, reply swallowed.
    Swallow,
    /// Never sent upstream; the proxy answers with this response code.
    Synth(u8),
    /// Refused by the safety interlock.
    Blocked,
    /// The DAC's hello on connect.
    Hello,
}

struct Pending {
    seq: u64,
    conn: u32,
    cmd: Vec<u8>,
    start: Instant,
    kind: Kind,
    note: String,
}

/// One recorded exchange, kept for verdicts.
#[derive(Clone, Debug)]
struct Ev {
    t: Instant,
    conn: u32,
    op: u8,
    rate: Option<u32>,
    points: usize,
    kind: Kind,
    resp: Option<DacResponse>,
}

impl Ev {
    fn st(&self) -> Option<&DacStatus> {
        self.resp.as_ref().map(|r| &r.dac_status)
    }
    fn ack(&self) -> bool {
        self.resp.is_some_and(|r| r.response == DacResponse::ACK)
    }
    fn code(&self) -> char {
        self.resp.map_or('-', |r| r.response as char)
    }
    fn playing(&self) -> bool {
        self.st()
            .is_some_and(|s| s.playback_state == DacStatus::PLAYBACK_PLAYING)
    }
    fn idle(&self) -> bool {
        self.st()
            .is_some_and(|s| s.playback_state == DacStatus::PLAYBACK_IDLE)
    }
    fn on_wire(&self) -> bool {
        matches!(self.kind, Kind::Forward | Kind::Swallow)
    }
}

struct ProxyState {
    events: Vec<Ev>,
    last: Option<DacStatus>,
    /// Fullness and point count of the last data reply, and when it came.
    last_data: Option<(u16, usize, Instant)>,
    estopped: bool,
    estop_seq: u64,
    reestop_after_clear: u32,
    aborted: Option<String>,
    allow_busy_hello: bool,
    conns: u32,
    outstanding: usize,
    /// Longest time from reading a DAC reply to handing it to the backend,
    /// and when it happened.
    max_forward_lag: (Duration, Instant),
}

impl Default for ProxyState {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            last: None,
            last_data: None,
            estopped: false,
            estop_seq: 0,
            reestop_after_clear: 0,
            aborted: None,
            allow_busy_hello: false,
            conns: 0,
            outstanding: 0,
            max_forward_lag: (Duration::ZERO, Instant::now()),
        }
    }
}

struct Conn {
    id: u32,
    up: TcpStream,
    tx: mpsc::Sender<Pending>,
}

struct Proxy {
    addr: SocketAddr,
    upstream: SocketAddr,
    check_lsof: bool,
    st: Mutex<ProxyState>,
    rec: Mutex<Option<Recorder>>,
    conn: Mutex<Option<Conn>>,
    seq: AtomicU64,
}

impl Proxy {
    fn start(upstream: SocketAddr, check_lsof: bool) -> io::Result<Arc<Self>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let proxy = Arc::new(Self {
            addr: listener.local_addr()?,
            upstream,
            check_lsof,
            st: Mutex::default(),
            rec: Mutex::default(),
            conn: Mutex::default(),
            seq: AtomicU64::new(1),
        });
        let p = proxy.clone();
        thread::spawn(move || {
            for client in listener.incoming().flatten() {
                p.open(client);
            }
        });
        Ok(proxy)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ProxyState> {
        self.st.lock().unwrap()
    }

    fn abort(&self, why: String) {
        eprintln!("ABORT: {why}");
        self.state().aborted.get_or_insert(why);
    }

    fn record(&self, p: &Pending, resp: &[u8], status: &'static str, done: Instant) {
        let op = p.cmd.first().copied().unwrap_or(b'?');
        let name = if p.kind == Kind::Hello {
            "hello"
        } else {
            op_name(op)
        };
        let mut note = p.note.clone();
        let mut add = |s: String| {
            if !note.is_empty() {
                note.push_str("; ");
            }
            note.push_str(&s);
        };
        match p.kind {
            Kind::Swallow => add("injected by the proxy, reply not forwarded".into()),
            Kind::Synth(_) => {
                add("not sent: DAC in e-stop; NAK synthesized from the last real status".into())
            }
            Kind::Blocked => add("not sent: refused by the safety interlock".into()),
            _ => {}
        }
        let points = if op == b'd' && p.kind != Kind::Hello {
            add(format!(
                "payload elided: {} blanked points, 18 zero bytes each",
                (p.cmd.len() - 3) / 18
            ));
            (p.cmd.len() - 3) / 18
        } else {
            0
        };
        let req: &[u8] = match p.kind {
            Kind::Hello => &[],
            _ if op == b'd' => &p.cmd[..3],
            _ => &p.cmd,
        };
        let n = if p.kind == Kind::Hello {
            0
        } else {
            p.cmd.len()
        };
        if let Some(rec) = self.rec.lock().unwrap().as_mut() {
            rec.event(name, req, n, resp, status, p.start, done, &note);
        }
        let resp = parse_resp(resp);
        let mut st = self.state();
        if let Some(r) = resp {
            if matches!(p.kind, Kind::Forward | Kind::Swallow | Kind::Hello) {
                st.last = Some(r.dac_status);
                if op == b'd' {
                    st.last_data = Some((r.dac_status.buffer_fullness, points, done));
                }
                if p.seq > st.estop_seq {
                    st.estopped = r.dac_status.light_engine_state != DacStatus::LIGHT_ENGINE_READY;
                }
            }
        }
        st.events.push(Ev {
            t: done,
            conn: p.conn,
            op,
            rate: rate_of(&p.cmd),
            points,
            kind: p.kind,
            resp,
        });
    }

    /// Why the DAC may have another client, if it may. Fails closed: an
    /// `lsof` that cannot run or reports an error counts as a reason.
    fn lsof_others(&self) -> Option<String> {
        if !self.check_lsof {
            return None;
        }
        let port = self.upstream.port();
        let out = match std::process::Command::new("lsof")
            .args(["-nP", &format!("-iTCP:{port}"), "-Fpcn"])
            .output()
        {
            Ok(out) => out,
            Err(e) => {
                return Some(format!(
                    "could not run lsof to check for other clients: {e}"
                ))
            }
        };
        // lsof exits 1 when nothing matches, possibly after printing
        // warnings such as an unreachable network mount (indented lines
        // continue the line above). Any other exit status, or a failure with
        // a stderr line that is not a warning, is an error we cannot rule out.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let errors = stderr.lines().any(|l| {
            !l.trim().is_empty()
                && !l.starts_with(char::is_whitespace)
                && !l.starts_with("lsof: WARNING")
        });
        if !out.status.success() && (out.status.code() != Some(1) || errors) {
            return Some(format!("lsof failed ({}): {}", out.status, stderr.trim()));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let me = std::process::id();
        let others: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with('p'))
            .filter(|l| l[1..].parse::<u32>().ok() != Some(me))
            .collect();
        (!others.is_empty()).then(|| format!("lsof shows other processes on TCP {port}: {text}"))
    }

    fn open(self: &Arc<Self>, client: TcpStream) {
        if self.state().aborted.is_some() {
            return;
        }
        if let Some(why) = self.lsof_others() {
            self.abort(why);
            return;
        }
        let up = match TcpStream::connect_timeout(&self.upstream, UPSTREAM_TIMEOUT) {
            Ok(s) => s,
            Err(e) => {
                self.abort(format!("upstream connect failed: {e}"));
                return;
            }
        };
        let _ = up.set_nodelay(true);
        let _ = up.set_read_timeout(Some(UPSTREAM_TIMEOUT));
        let _ = client.set_nodelay(true);
        let id = {
            let mut st = self.state();
            st.conns += 1;
            st.conns
        };
        let hello = Pending {
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
            conn: id,
            cmd: vec![b'?'],
            start: Instant::now(),
            kind: Kind::Hello,
            note: String::new(),
        };
        let (resp, status) = read_reply(&up, DacResponse::SIZE_BYTES);
        let parsed = parse_resp(&resp);
        let allow_busy = std::mem::take(&mut self.state().allow_busy_hello);
        let busy = parsed.map(|r| r.dac_status.playback_state != DacStatus::PLAYBACK_IDLE);
        let mut hello = hello;
        if busy == Some(true) {
            hello.note = if allow_busy {
                "not idle: left over from this tool's own session".into()
            } else {
                "ABORT: DAC busy at hello".into()
            };
        }
        self.record(&hello, &resp, status, Instant::now());
        match busy {
            Some(false) => {}
            Some(true) if allow_busy => {}
            Some(true) => {
                let s = parsed.unwrap().dac_status;
                self.abort(format!(
                    "DAC not idle at hello (playback {}, rate {}, fullness {}): another client \
                     is probably streaming",
                    s.playback_state, s.point_rate, s.buffer_fullness
                ));
                let _ = up.shutdown(Shutdown::Both);
                return;
            }
            None => {
                self.abort(format!("no valid hello ({status})"));
                let _ = up.shutdown(Shutdown::Both);
                return;
            }
        }
        let mut client_w = client.try_clone().expect("clone");
        if client_w.write_all(&resp).is_err() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        *self.conn.lock().unwrap() = Some(Conn {
            id,
            up: up.try_clone().expect("clone"),
            tx: tx.clone(),
        });
        let p = self.clone();
        let up_r = up.try_clone().expect("clone");
        thread::spawn(move || p.reader(id, up_r, client_w, rx));
        let p = self.clone();
        thread::spawn(move || p.writer(id, client, tx));
    }

    /// Send `cmd` upstream under the connection lock. Returns false if there
    /// is no live connection.
    fn send_locked(&self, conn: &mut Conn, cmd: Vec<u8>, kind: Kind, note: &str) -> bool {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        if cmd[0] == 0x00 || cmd[0] == 0xff {
            let mut st = self.state();
            st.estopped = true;
            st.estop_seq = seq;
        }
        self.state().outstanding += 1;
        let bytes = if matches!(kind, Kind::Synth(_)) {
            None
        } else {
            Some(cmd.clone())
        };
        let p = Pending {
            seq,
            conn: conn.id,
            cmd,
            start: Instant::now(),
            kind,
            note: note.into(),
        };
        if conn.tx.send(p).is_err() {
            self.settle();
            return false;
        }
        match bytes {
            Some(b) => conn.up.write_all(&b).is_ok(),
            None => true,
        }
    }

    /// Whether `op` must be held back because the DAC is in e-stop. Only
    /// ping, clear and e-stop itself reach a DAC in e-stop.
    fn estop_gated(&self, op: u8) -> bool {
        self.state().estopped && !matches!(op, b'?' | b'c' | 0x00 | 0xff)
    }

    /// Inject commands into the live connection; their replies are recorded
    /// and swallowed. Every command passes the same safety checks. A command
    /// the e-stop gate holds back has no client to take a synthesized reply,
    /// so it is recorded as blocked (failing the scenario) and not sent.
    fn inject(&self, cmds: &[Vec<u8>], note: &str) -> bool {
        let mut guard = self.conn.lock().unwrap();
        let Some(conn) = guard.as_mut() else {
            return false;
        };
        for c in cmds {
            if let Some(why) = unsafe_reason(c) {
                panic!("refusing to inject unsafe command: {why}");
            }
            if self.estop_gated(c[0]) {
                let why = format!("injected {} while the DAC is in e-stop", op_name(c[0]));
                eprintln!("interlock: {why}");
                let p = Pending {
                    seq: self.seq.fetch_add(1, Ordering::SeqCst),
                    conn: conn.id,
                    cmd: c.clone(),
                    start: Instant::now(),
                    kind: Kind::Blocked,
                    note: why,
                };
                self.record(&p, &[], "blocked", Instant::now());
                return false;
            }
            if !self.send_locked(conn, c.clone(), Kind::Swallow, note) {
                return false;
            }
        }
        true
    }

    fn writer(self: Arc<Self>, id: u32, mut client: TcpStream, tx: mpsc::Sender<Pending>) {
        let mut buf = Vec::new();
        let mut tmp = vec![0u8; 1 << 16];
        'outer: loop {
            let n = match client.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            loop {
                let len = match cmd_len(&buf) {
                    None => break,
                    Some(Ok(len)) => len,
                    Some(Err(op)) => {
                        self.refuse(id, buf.clone(), format!("unknown opcode 0x{op:02x}"));
                        break 'outer;
                    }
                };
                if buf.len() < len {
                    break;
                }
                let cmd: Vec<u8> = buf.drain(..len).collect();
                if let Some(why) = unsafe_reason(&cmd) {
                    self.refuse(id, cmd, why);
                    break 'outer;
                }
                let mut guard = self.conn.lock().unwrap();
                let Some(conn) = guard.as_mut().filter(|c| c.id == id) else {
                    break 'outer;
                };
                let kind = if self.estop_gated(cmd[0]) {
                    Kind::Synth(DacResponse::NAK_INVALID)
                } else {
                    Kind::Forward
                };
                if !self.send_locked(conn, cmd, kind, "") {
                    break 'outer;
                }
            }
        }
        // A trailing partial command is dropped, never forwarded.
        let mut guard = self.conn.lock().unwrap();
        if guard.as_ref().is_some_and(|c| c.id == id) {
            *guard = None;
        }
        drop(guard);
        drop(tx);
    }

    /// Record a refused command, bring the DAC back to idle on this
    /// connection, and close it.
    fn refuse(&self, id: u32, cmd: Vec<u8>, why: String) {
        eprintln!("interlock: {why}");
        let p = Pending {
            seq: self.seq.fetch_add(1, Ordering::SeqCst),
            conn: id,
            cmd,
            start: Instant::now(),
            kind: Kind::Blocked,
            note: why,
        };
        self.record(&p, &[], "blocked", Instant::now());
        let last = self.state().last;
        if let Some(st) = last {
            let mut guard = self.conn.lock().unwrap();
            if let Some(conn) = guard.as_mut().filter(|c| c.id == id) {
                for c in stop_cmds(&st) {
                    self.send_locked(conn, c, Kind::Swallow, "interlock: stop before closing");
                }
            }
        }
    }

    fn reader(
        self: Arc<Self>,
        id: u32,
        up: TcpStream,
        mut client: TcpStream,
        rx: mpsc::Receiver<Pending>,
    ) {
        while let Ok(p) = rx.recv() {
            let op = p.cmd[0];
            if let Kind::Synth(code) = p.kind {
                let last = self.state().last.expect("status known after hello");
                let bytes = resp_bytes(DacResponse {
                    response: code,
                    command: op,
                    dac_status: last,
                });
                self.record(&p, &bytes, "synthesized", Instant::now());
                self.settle();
                let _ = client.write_all(&bytes);
                continue;
            }
            let (mut resp, mut status) = read_reply(&up, DacResponse::SIZE_BYTES);
            if op == b'v' && status == "ok" {
                let nak = matches!(resp[0], b'F' | b'I' | b'!') && resp[1] == b'v';
                if !nak {
                    let (rest, st2) = read_reply(&up, 32 - DacResponse::SIZE_BYTES);
                    resp.extend_from_slice(&rest);
                    status = st2;
                }
            }
            let done = Instant::now();
            self.record(&p, &resp, status, done);
            // Re-engage the e-stop right after a successful clear, before the
            // backend can send anything else, to exercise its retry spacing.
            if op == b'c' && status == "ok" && p.kind == Kind::Forward {
                let again = {
                    let mut st = self.state();
                    let go = st.reestop_after_clear > 0;
                    if go {
                        st.reestop_after_clear -= 1;
                    }
                    go
                };
                if again {
                    let mut guard = self.conn.lock().unwrap();
                    if let Some(conn) = guard.as_mut().filter(|c| c.id == id) {
                        self.send_locked(conn, vec![0x00], Kind::Swallow, "re-engage e-stop");
                    }
                }
            }
            self.settle();
            if status != "ok" {
                break;
            }
            if p.kind == Kind::Forward {
                let _ = client.write_all(&resp);
                let lag = done.elapsed();
                let mut st = self.state();
                if lag > st.max_forward_lag.0 {
                    st.max_forward_lag = (lag, done);
                }
            }
        }
        // Drain anything left so `outstanding` settles.
        while let Ok(p) = rx.try_recv() {
            drop(p);
            self.settle();
        }
        let _ = up.shutdown(Shutdown::Both);
        let _ = client.shutdown(Shutdown::Both);
    }

    /// One outstanding reply has been accounted for. Saturating, because
    /// `Ctx::begin` zeroes the counter between scenarios.
    fn settle(&self) {
        let mut st = self.state();
        st.outstanding = st.outstanding.saturating_sub(1);
    }

    /// Wait until no connection is open and no reply is outstanding.
    fn wait_quiet(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.conn.lock().unwrap().is_none() && self.state().outstanding == 0 {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn wait_replies(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.state().outstanding > 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

fn read_reply(sock: &TcpStream, len: usize) -> (Vec<u8>, &'static str) {
    let mut sock = sock;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        match sock.read(&mut buf[got..]) {
            Ok(0) => return (buf[..got].to_vec(), "closed"),
            Ok(n) => got += n,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return (buf[..got].to_vec(), "timeout")
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                return (buf[..got].to_vec(), "reset")
            }
            Err(_) => return (buf[..got].to_vec(), "io"),
        }
    }
    (buf, "ok")
}

// --- Raw client through the proxy ---------------------------------------------------

struct Raw {
    sock: TcpStream,
    hello: DacResponse,
}

impl Raw {
    fn connect(addr: SocketAddr) -> Result<Self, String> {
        let sock = TcpStream::connect(addr).map_err(|e| e.to_string())?;
        sock.set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|e| e.to_string())?;
        let (resp, status) = read_reply(&sock, DacResponse::SIZE_BYTES);
        let hello = parse_resp(&resp).ok_or(format!("proxy refused the connection ({status})"))?;
        Ok(Self { sock, hello })
    }

    fn cmd(&mut self, bytes: &[u8]) -> Result<DacResponse, String> {
        self.sock.write_all(bytes).map_err(|e| e.to_string())?;
        let (resp, status) = read_reply(&self.sock, DacResponse::SIZE_BYTES);
        parse_resp(&resp).ok_or(format!("{} failed ({status})", op_name(bytes[0])))
    }
}

// --- Backend wrapper ----------------------------------------------------------------

#[derive(Clone, Debug)]
struct WriteLog {
    t: Instant,
    pps: u32,
    n: usize,
    est_before: u64,
    outcome: Result<WriteOutcome, String>,
    /// DAC fullness from the data reply to this write, if one came.
    dac_after: Option<u16>,
    /// How long the backend call took.
    dur: Duration,
}

#[derive(Default)]
struct TapShared {
    pause_until: Mutex<Option<Instant>>,
    log: Mutex<Vec<WriteLog>>,
}

/// Wraps the real backend. Forces every point to a blanked centre point,
/// logs each write with the estimator's view, and can withhold writes to
/// simulate a stalled host.
struct Tap {
    inner: EtherDreamBackend,
    shared: Arc<TapShared>,
    proxy: Arc<Proxy>,
    blank: Vec<LaserPoint>,
}

impl DacBackend for Tap {
    fn dac_type(&self) -> DacType {
        self.inner.dac_type()
    }
    fn caps(&self) -> &DacCapabilities {
        self.inner.caps()
    }
    fn connect(&mut self) -> laser_dac::Result<()> {
        self.inner.connect()
    }
    fn disconnect(&mut self) -> laser_dac::Result<()> {
        self.inner.disconnect()
    }
    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }
    fn stop(&mut self) -> laser_dac::Result<()> {
        self.inner.stop()
    }
    fn set_shutter(&mut self, open: bool) -> laser_dac::Result<()> {
        self.inner.set_shutter(open)
    }
}

impl FifoBackend for Tap {
    fn try_write_points(
        &mut self,
        pps: u32,
        points: &[LaserPoint],
    ) -> laser_dac::Result<WriteOutcome> {
        let t = Instant::now();
        let paused = self
            .shared
            .pause_until
            .lock()
            .unwrap()
            .is_some_and(|u| t < u);
        let est_before = self.inner.estimator().estimated_fullness(t, pps);
        let outcome = if paused {
            Ok(WriteOutcome::WouldBlock)
        } else {
            self.blank.clear();
            self.blank
                .resize(points.len(), LaserPoint::blanked(0.0, 0.0));
            self.inner.try_write_points(pps, &self.blank)
        };
        let dac_after = self
            .proxy
            .state()
            .last_data
            .filter(|&(_, _, at)| at >= t)
            .map(|(f, _, _)| f);
        self.shared.log.lock().unwrap().push(WriteLog {
            t,
            pps,
            n: points.len(),
            est_before,
            outcome: outcome.as_ref().map(|o| *o).map_err(|e| e.to_string()),
            dac_after,
            dur: t.elapsed(),
        });
        outcome
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        self.inner.estimator()
    }

    fn target_buffer_ceiling(&self) -> Option<usize> {
        self.inner.target_buffer_ceiling()
    }
}

// --- Scenario harness ---------------------------------------------------------------

struct Ctx {
    proxy: Arc<Proxy>,
    bc: DacBroadcast,
    sim: bool,
    out: PathBuf,
    prefix: String,
    steady: Duration,
    steady_pps: u32,
    steady_target: Option<Duration>,
}

struct Verdict {
    name: String,
    pass: Option<bool>,
    metrics: Value,
    notes: Vec<String>,
}

impl Verdict {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            pass: Some(true),
            metrics: json!({}),
            notes: vec![],
        }
    }
    fn check(&mut self, ok: bool, what: impl Into<String>) {
        let what = what.into();
        if !ok {
            self.pass = Some(false);
            self.notes.push(format!("FAIL: {what}"));
        } else {
            self.notes.push(format!("ok: {what}"));
        }
    }
    fn set(&mut self, key: &str, v: Value) {
        self.metrics[key] = v;
    }
}

struct StreamRun {
    exit: String,
    errors: Vec<String>,
    log: Vec<WriteLog>,
}

fn blanks(n: usize) -> Vec<LaserPoint> {
    vec![LaserPoint::blanked(0.0, 0.0); n]
}

impl Ctx {
    fn tap(&self) -> (Tap, Arc<TapShared>) {
        let shared = Arc::new(TapShared::default());
        let tap = Tap {
            inner: EtherDreamBackend::with_address(self.proxy.addr, Some(self.bc)),
            shared: shared.clone(),
            proxy: self.proxy.clone(),
            blank: Vec::new(),
        };
        (tap, shared)
    }

    fn begin(&self, name: &str) -> io::Result<()> {
        let path = self.out.join(format!("{}_{name}.jsonl", self.prefix));
        *self.proxy.rec.lock().unwrap() = Some(Recorder::new(&path, name)?);
        let mut st = self.proxy.state();
        st.events.clear();
        st.reestop_after_clear = 0;
        st.estopped = false;
        // A reply lost when a connection died must not make later scenarios
        // wait on it.
        st.outstanding = 0;
        st.max_forward_lag = (Duration::ZERO, Instant::now());
        Ok(())
    }

    fn finish(&self) {
        if let Some(rec) = self.proxy.rec.lock().unwrap().take() {
            if let Err(e) = rec.save() {
                eprintln!("could not write {}: {e}", rec.path.display());
            }
        }
    }

    fn events(&self) -> Vec<Ev> {
        self.proxy.state().events.clone()
    }

    /// Run the presentation-layer stream on `tap` (never armed, so every
    /// point is a parked blank) while `script` drives it from another thread.
    fn stream(
        &self,
        tap: Tap,
        pps: u32,
        script: impl FnOnce(SessionControl) + Send + 'static,
    ) -> Result<StreamRun, String> {
        self.stream_cfg(tap, StreamConfig::new(pps), script)
    }

    fn stream_cfg(
        &self,
        tap: Tap,
        cfg: StreamConfig,
        script: impl FnOnce(SessionControl) + Send + 'static,
    ) -> Result<StreamRun, String> {
        let shared = tap.shared.clone();
        let backend = BackendKind::fifo(Box::new(tap)).map_err(|e| e.to_string())?;
        let caps = backend.caps().clone();
        let info = DacInfo::new("etherdream:hwcheck", "hwcheck", DacType::EtherDream, caps);
        let (stream, _) = Dac::new(info, backend)
            .start_stream(cfg)
            .map_err(|e| e.to_string())?;
        let control = stream.control();
        assert!(
            !control.is_armed(),
            "the hwcheck stream must never be armed"
        );
        let driver = thread::spawn(move || script(control));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let errs = errors.clone();
        let exit = stream.run(
            |req: &ChunkRequest, buf: &mut [LaserPoint]| {
                let n = req.target_points.min(buf.len());
                buf[..n].fill(LaserPoint::blanked(0.0, 0.0));
                ChunkResult::Filled(n)
            },
            move |e| errs.lock().unwrap().push(e.to_string()),
        );
        let _ = driver.join();
        self.proxy.wait_quiet(Duration::from_secs(3));
        let log = shared.log.lock().unwrap().clone();
        let errors = errors.lock().unwrap().clone();
        self.dump_writes(&log);
        Ok(StreamRun {
            exit: format!("{exit:?}"),
            errors,
            log,
        })
    }

    /// Write the per-call log of `try_write_points` (estimate before the
    /// call, outcome, DAC fullness from the data reply) next to the trace.
    fn dump_writes(&self, log: &[WriteLog]) {
        let Some(phase) = self
            .proxy
            .rec
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| (r.phase.clone(), r.t0))
        else {
            return;
        };
        let path = self
            .out
            .join(format!("{}_{}.writes.jsonl", self.prefix, phase.0));
        let Ok(f) = File::create(path) else { return };
        let mut f = BufWriter::new(f);
        let mut last: Option<(&str, u32)> = None;
        for w in log {
            let outcome = match &w.outcome {
                Ok(WriteOutcome::Written) => "written",
                Ok(WriteOutcome::WouldBlock) => "would_block",
                Err(_) => "error",
            };
            // Collapse runs of identical WouldBlock calls.
            if outcome == "would_block" && last == Some((outcome, w.pps)) {
                continue;
            }
            last = Some((outcome, w.pps));
            let _ = writeln!(
                f,
                "{}",
                json!({
                    "t_us": w.t.saturating_duration_since(phase.1).as_micros() as u64,
                    "pps": w.pps, "n": w.n, "est_before": w.est_before,
                    "outcome": outcome, "dac_after": w.dac_after,
                    "dur_us": w.dur.as_micros() as u64,
                    "error": w.outcome.as_ref().err(),
                })
            );
        }
    }

    /// Confirm the DAC is idle with a fresh hello, stopping it first if this
    /// tool's own session left it running.
    fn leave_idle(&self, v: &mut Verdict) {
        self.proxy.wait_quiet(Duration::from_secs(3));
        // A stream ended by SessionControl::stop() should already have sent
        // 's'; give any ring left playing time to play out on its own.
        thread::sleep(Duration::from_millis(300));
        let last = self.proxy.state().last;
        if last.is_some_and(|s| s.playback_state != DacStatus::PLAYBACK_IDLE) {
            self.proxy.state().allow_busy_hello = true;
        }
        match Raw::connect(self.proxy.addr) {
            Ok(mut raw) => {
                for c in stop_cmds(&raw.hello.dac_status) {
                    let _ = raw.cmd(&c);
                }
                let idle = self.proxy.state().last.map(|s| s.playback_state);
                v.check(idle == Some(DacStatus::PLAYBACK_IDLE), "DAC left idle");
            }
            Err(e) => v.check(false, format!("final idle check: {e}")),
        }
        self.proxy.state().allow_busy_hello = false;
        self.proxy.wait_quiet(Duration::from_secs(3));
    }
}

fn first_conn_after(events: &[Ev], conn: u32) -> Vec<Ev> {
    events.iter().filter(|e| e.conn > conn).cloned().collect()
}

fn est_error_stats(log: &[WriteLog], pps: Option<u32>) -> Value {
    let errs: Vec<i64> = log
        .iter()
        .filter(|w| matches!(w.outcome, Ok(WriteOutcome::Written)))
        .filter(|w| pps.is_none_or(|p| w.pps == p))
        .filter_map(|w| {
            w.dac_after
                .map(|f| w.est_before as i64 - (f as i64 - w.n as i64))
        })
        .collect();
    stats(&errs)
}

fn stats(v: &[i64]) -> Value {
    if v.is_empty() {
        return json!({"count": 0});
    }
    let min = *v.iter().min().unwrap();
    let max = *v.iter().max().unwrap();
    let mean = v.iter().sum::<i64>() as f64 / v.len() as f64;
    json!({"count": v.len(), "min": min, "max": max, "mean": (mean * 10.0).round() / 10.0})
}

/// The largest absolute value in `stats`, or `None` when there were no
/// samples, so a check over an empty set fails instead of passing.
fn max_abs(v: &Value) -> Option<i64> {
    if v["count"].as_u64().unwrap_or(0) == 0 {
        return None;
    }
    let a = v["min"].as_i64().unwrap_or(0).abs();
    let b = v["max"].as_i64().unwrap_or(0).abs();
    Some(a.max(b))
}

/// Where the host was slow: the longest backend call, the longest pause
/// between call starts, and the longest delay inside the proxy.
fn host_timing(cx: &Ctx, log: &[WriteLog], t0: Instant) -> Value {
    let at = |t: Instant| ms_between(t0, t);
    let call = log.iter().max_by_key(|w| w.dur);
    let gap = log
        .windows(2)
        .max_by_key(|w| w[1].t.saturating_duration_since(w[0].t));
    let (lag, lag_at) = cx.proxy.state().max_forward_lag;
    json!({
        "longest_backend_call_ms": call.map(|w| w.dur.as_secs_f64() * 1000.0),
        "longest_backend_call_at_ms": call.map(|w| at(w.t)),
        "longest_gap_between_calls_ms": gap.map(|w| ms_between(w[0].t, w[1].t)),
        "longest_gap_at_ms": gap.map(|w| at(w[0].t)),
        "longest_proxy_forward_ms": lag.as_secs_f64() * 1000.0,
        "longest_proxy_forward_at_ms": at(lag_at),
    })
}

fn ms_between(a: Instant, b: Instant) -> f64 {
    (b.saturating_duration_since(a).as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

// --- Scenarios ------------------------------------------------------------------------

fn stale_full_reconnect(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("stale_full_reconnect");
    let cap = cx.bc.buffer_capacity as usize;
    {
        let mut raw = Raw::connect(cx.proxy.addr)?;
        raw.cmd(b"p")?;
        raw.cmd(&cmd::blank_data(1000))?;
        raw.cmd(&cmd::blank_data(cap - 1000))?;
        raw.cmd(&cmd::begin(1_000))?;
        let s = raw.cmd(b"s")?;
        v.set(
            "stale_fullness_after_stop",
            json!(s.dac_status.buffer_fullness),
        );
        v.check(
            s.dac_status.playback_state == DacStatus::PLAYBACK_IDLE
                && s.dac_status.buffer_fullness as usize + 50 >= cap,
            format!(
                "setup leaves an idle DAC with a stale full ring ({} points)",
                s.dac_status.buffer_fullness
            ),
        );
    }
    cx.proxy.wait_quiet(Duration::from_secs(3));
    let raw_conn = cx.proxy.state().conns;
    let (tap, _) = cx.tap();
    let run = cx.stream(tap, 1_000, |c| {
        thread::sleep(Duration::from_millis(1500));
        let _ = c.stop();
    })?;
    let evs = first_conn_after(&cx.events(), raw_conn);
    let hello = evs
        .iter()
        .find(|e| e.kind == Kind::Hello)
        .ok_or("no backend hello")?;
    v.set(
        "hello_fullness",
        json!(hello.st().map(|s| s.buffer_fullness)),
    );
    let first_cmd = evs
        .iter()
        .find(|e| e.kind == Kind::Forward && e.op != b'v')
        .ok_or("backend sent nothing")?;
    v.check(
        first_cmd.op == b'p',
        format!("first command after connect is {}", op_name(first_cmd.op)),
    );
    let playing = evs.iter().find(|e| e.playing());
    let to_play = playing.map(|p| ms_between(hello.t, p.t));
    v.set("ms_hello_to_playing", json!(to_play));
    v.check(
        to_play.is_some_and(|ms| ms <= 1000.0),
        format!("reached Playing within 1 s ({to_play:?} ms)"),
    );
    let blocks_before_first = run
        .log
        .iter()
        .take_while(|w| !matches!(w.outcome, Ok(WriteOutcome::Written)))
        .count();
    v.set("wouldblock_before_first_write", json!(blocks_before_first));
    v.check(
        run.errors.is_empty(),
        format!("no stream errors {:?}", run.errors),
    );
    v.set("exit", json!(run.exit));
    cx.leave_idle(&mut v);
    Ok(v)
}

fn underflow_recovery(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("underflow_recovery");
    let (tap, shared) = cx.tap();
    let pause_at = Arc::new(Mutex::new(None));
    let pa = pause_at.clone();
    let run = cx.stream(tap, 1_000, move |c| {
        thread::sleep(Duration::from_millis(1000));
        let now = Instant::now();
        *pa.lock().unwrap() = Some(now);
        *shared.pause_until.lock().unwrap() = Some(now + Duration::from_millis(400));
        thread::sleep(Duration::from_millis(1600));
        let _ = c.stop();
    })?;
    let pause = pause_at.lock().unwrap().ok_or("pause never started")?;
    let evs = cx.events();
    let after: Vec<&Ev> = evs.iter().filter(|e| e.t >= pause).collect();
    let underflow = after.iter().find(|e| {
        e.idle()
            && e.st()
                .is_some_and(|s| s.playback_flags & PBF_UNDERFLOW != 0)
    });
    v.check(
        underflow.is_some(),
        "DAC reported idle with the underflow flag",
    );
    let Some(u) = underflow else {
        return Ok(v);
    };
    v.set(
        "underflow_seen_by",
        json!(format!("{} -> {}", op_name(u.op), u.code())),
    );
    let later: Vec<&&Ev> = after.iter().filter(|e| e.t >= u.t).collect();
    let prep = later.iter().position(|e| e.op == b'p' && e.ack());
    let begin = later.iter().position(|e| e.op == b'b' && e.ack());
    let play = later.iter().position(|e| e.playing());
    v.check(prep.is_some(), "re-prepared after the underflow");
    v.check(
        begin.is_some() && begin > prep,
        "re-began after the re-prepare",
    );
    v.check(play.is_some(), "reached Playing again");
    if let Some(i) = play {
        v.set(
            "ms_underflow_to_playing",
            json!(ms_between(u.t, later[i].t)),
        );
    }
    let acked_dropped: usize = evs
        .iter()
        .filter(|e| e.op == b'd' && e.ack() && e.idle())
        .map(|e| e.points)
        .sum();
    let naked: usize = evs
        .iter()
        .filter(|e| e.op == b'd' && !e.ack() && e.on_wire())
        .map(|e| e.points)
        .sum();
    v.set("points_acked_while_idle", json!(acked_dropped));
    v.set("points_naked_and_resent", json!(naked));
    v.set(
        "seq_after_underflow",
        json!(later
            .iter()
            .take(6)
            .map(|e| format!("{}:{}", op_name(e.op), e.code()))
            .collect::<Vec<_>>()),
    );
    v.check(
        run.errors.is_empty(),
        format!("no stream errors {:?}", run.errors),
    );
    v.check(cx.proxy.state().conns >= 1, "connection count");
    v.set("exit", json!(run.exit));
    cx.leave_idle(&mut v);
    Ok(v)
}

fn estop_recovery(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("estop_recovery");
    let conn0 = cx.proxy.state().conns;
    let (tap, _) = cx.tap();
    let proxy = cx.proxy.clone();
    let at = Arc::new(Mutex::new(None));
    let at2 = at.clone();
    let run = cx.stream(tap, 1_000, move |c| {
        thread::sleep(Duration::from_millis(1000));
        proxy.state().reestop_after_clear = 1;
        *at2.lock().unwrap() = Some(Instant::now());
        proxy.inject(&[vec![0x00]], "e-stop");
        thread::sleep(Duration::from_millis(3000));
        let _ = c.stop();
    })?;
    let t_estop = at.lock().unwrap().ok_or("no e-stop injected")?;
    let evs: Vec<Ev> = cx.events().into_iter().filter(|e| e.t >= t_estop).collect();
    let estop_ack = evs
        .iter()
        .find(|e| e.op == 0x00)
        .and_then(|e| e.st().copied());
    v.check(
        estop_ack.is_some_and(|s| s.light_engine_state == DacStatus::LIGHT_ENGINE_EMERGENCY_STOP),
        "DAC entered e-stop",
    );
    let clears: Vec<&Ev> = evs.iter().filter(|e| e.op == b'c' && e.on_wire()).collect();
    let gaps: Vec<f64> = clears
        .windows(2)
        .map(|w| ms_between(w[0].t, w[1].t))
        .collect();
    v.set("clear_count", json!(clears.len()));
    v.set("clear_gaps_ms", json!(gaps));
    v.check(!clears.is_empty(), "backend sent clear-e-stop");
    v.check(
        gaps.iter().all(|&g| g >= 900.0),
        "clear attempts at least ~1 s apart",
    );
    let synth: Vec<String> = evs
        .iter()
        .filter(|e| matches!(e.kind, Kind::Synth(_)))
        .map(|e| op_name(e.op).to_string())
        .collect();
    v.set("commands_withheld_during_estop", json!(synth));
    let last_clear = clears.last().map(|e| e.t);
    let resumed = last_clear.and_then(|t| evs.iter().find(|e| e.t > t && e.playing()));
    v.check(resumed.is_some(), "playback resumed after the clear");
    if let (Some(r), Some(c)) = (resumed, last_clear) {
        v.set("ms_clear_to_playing", json!(ms_between(c, r.t)));
        let flag_after = evs
            .iter()
            .find(|e| e.t > c && e.op == b'p')
            .and_then(|e| e.st().map(|s| s.playback_flags & PBF_ESTOP));
        v.set("estop_flag_after_prepare", json!(flag_after));
    }
    let wire_during: usize = evs
        .iter()
        .filter(|e| e.on_wire() && last_clear.is_some_and(|c| e.t <= c))
        .count();
    v.set("commands_on_wire_estop_to_last_clear", json!(wire_during));
    let calls_during = run
        .log
        .iter()
        .filter(|w| w.t >= t_estop && last_clear.is_some_and(|c| w.t <= c))
        .count();
    v.set("try_write_calls_estop_to_last_clear", json!(calls_during));
    v.check(
        run.errors.is_empty(),
        format!("no fatal error {:?}", run.errors),
    );
    let conns = cx.proxy.state().conns - conn0;
    v.check(conns == 1, format!("no reconnect ({conns} connections)"));
    v.set("exit", json!(run.exit));
    cx.leave_idle(&mut v);
    Ok(v)
}

fn rate_change(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("rate_change");
    let t0 = cx
        .proxy
        .rec
        .lock()
        .unwrap()
        .as_ref()
        .map_or_else(Instant::now, |r| r.t0);
    let (tap, _) = cx.tap();
    // Each step: (pps, time set_pps returned). The stream starts at 1000.
    const STEPS: [u32; 4] = [20_000, 1_000, 30_000, 1_000];
    let changes: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::default();
    let marks = changes.clone();
    let run = cx.stream(tap, 1_000, move |c| {
        for pps in STEPS {
            thread::sleep(Duration::from_millis(1500));
            let r = c.set_pps(pps);
            marks.lock().unwrap().push((pps, Instant::now()));
            if r.is_err() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(1500));
        let _ = c.stop();
    })?;
    let changes = changes.lock().unwrap().clone();
    let evs = cx.events();
    let first_play = evs.iter().position(|e| e.playing()).ok_or("never played")?;
    let last_stop = evs[first_play..]
        .iter()
        .rposition(|e| e.op == b's')
        .map_or(evs.len(), |i| first_play + i);
    let window = &evs[first_play..last_stop];
    let underflows = window
        .iter()
        .filter(|e| {
            e.idle()
                || e.st()
                    .is_some_and(|s| s.playback_flags & PBF_UNDERFLOW != 0)
        })
        .count();
    v.check(
        underflows == 0,
        format!("no underflow ({underflows} idle replies)"),
    );
    // Longest pause between backend write calls while streaming: a downward
    // rate change reaches the DAC only with the next data write.
    let gap = run
        .log
        .windows(2)
        .map(|w| (ms_between(w[0].t, w[1].t), w[0].pps, w[1].pps))
        .fold((0.0, 0, 0), |a, b| if b.0 > a.0 { b } else { a });
    v.set(
        "longest_write_gap",
        json!({"ms": gap.0, "pps_before": gap.1, "pps_after": gap.2}),
    );
    v.set("host_timing", host_timing(cx, &run.log, t0));
    let updates: Vec<u32> = evs
        .iter()
        .filter(|e| e.op == b'u')
        .filter_map(|e| e.rate)
        .collect();
    v.set("update_rates", json!(updates));
    v.check(
        updates == STEPS,
        format!("update commands carried {STEPS:?} ({updates:?})"),
    );
    // The DAC's reported rate must follow each set_pps within 500 ms.
    let mut follow_ms = Vec::new();
    for &(pps, at) in &changes {
        let seen = evs
            .iter()
            .find(|e| e.t >= at && e.st().is_some_and(|s| s.point_rate == pps))
            .map(|e| ms_between(at, e.t));
        follow_ms.push(json!({"pps": pps, "ms": seen}));
        v.check(
            seen.is_some_and(|ms| ms <= 500.0),
            format!("status rate reaches {pps} within 500 ms ({seen:?} ms)"),
        );
    }
    v.set("status_rate_follow_ms", json!(follow_ms));
    let follows = evs
        .iter()
        .filter(|e| e.op == b'u')
        .all(|e| e.st().map(|s| s.point_rate) == e.rate);
    v.check(follows, "status point_rate follows each update");
    for pps in [1_000, 20_000, 30_000] {
        let s = est_error_stats(&run.log, Some(pps));
        v.set(&format!("estimate_error_at_{pps}"), s.clone());
        v.check(
            max_abs(&s).is_some_and(|m| m <= 300),
            format!("estimate within 300 points of the DAC at {pps} pps"),
        );
    }
    v.check(
        run.errors.is_empty(),
        format!("no stream errors {:?}", run.errors),
    );
    v.set("exit", json!(run.exit));
    cx.leave_idle(&mut v);
    Ok(v)
}

fn full_capacity_chunk(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("full_capacity_chunk");
    let cap = cx.bc.buffer_capacity as usize;
    let (mut tap, _) = cx.tap();
    tap.connect().map_err(|e| e.to_string())?;
    let o1 = tap.try_write_points(1_000, &blanks(cap));
    v.check(
        matches!(o1, Ok(WriteOutcome::Written)),
        format!("{cap}-point chunk written ({o1:?})"),
    );
    let evs = cx.events();
    let big = evs.iter().find(|e| e.op == b'd' && e.points == cap);
    v.check(
        big.is_some_and(|e| e.ack() && e.st().map(|s| s.buffer_fullness) == Some(cap as u16)),
        format!(
            "DAC ACKed it with fullness {cap} (got {:?} {:?})",
            big.map(|e| e.code()),
            big.and_then(|e| e.st().map(|s| s.buffer_fullness))
        ),
    );
    tap.stop().map_err(|e| e.to_string())?;

    // Fill the ring behind the backend's back, then write one more point.
    let o2 = tap.try_write_points(1_000, &blanks(10));
    v.check(
        matches!(o2, Ok(WriteOutcome::Written)),
        "10 points prepared",
    );
    cx.proxy.inject(
        &[cmd::blank_data(cap - 10)],
        "another client fills the ring",
    );
    cx.proxy.wait_replies(Duration::from_secs(2));
    let t = Instant::now();
    let o3 = tap.try_write_points(1_000, &blanks(1));
    v.check(
        matches!(o3, Ok(WriteOutcome::WouldBlock)),
        format!("overfull write returns WouldBlock ({o3:?})"),
    );
    let nak = cx.events().into_iter().find(|e| e.t >= t && e.op == b'd');
    v.set(
        "overfull_reply",
        json!(nak.as_ref().map(|e| format!(
            "{} fullness {:?}",
            e.code(),
            e.st().map(|s| s.buffer_fullness)
        ))),
    );
    v.check(
        nak.as_ref().is_some_and(|e| {
            e.code() == 'I' && e.st().map(|s| s.buffer_fullness) == Some(cap as u16)
        }),
        "DAC answered NAK-Invalid with a full ring",
    );
    let est = tap
        .inner
        .estimator()
        .estimated_fullness(Instant::now(), 1_000);
    v.set("estimate_after_nak", json!(est));
    v.check(
        est + 20 >= cap as u64,
        format!("estimator re-synced to the full ring ({est})"),
    );
    // The ring is Prepared and full, so waiting cannot make room. The next
    // write must begin playback instead of blocking forever.
    let t = Instant::now();
    let o4 = tap.try_write_points(1_000, &blanks(1));
    v.set("next_write_outcome", json!(format!("{o4:?}")));
    let after: Vec<Ev> = cx.events().into_iter().filter(|e| e.t >= t).collect();
    v.set(
        "next_write_commands",
        json!(after
            .iter()
            .map(|e| format!(
                "{} {} pb {:?} fullness {:?}",
                op_name(e.op),
                e.code(),
                e.st().map(|s| s.playback_state),
                e.st().map(|s| s.buffer_fullness)
            ))
            .collect::<Vec<_>>()),
    );
    let b = after.iter().find(|e| e.op == b'b');
    v.check(
        b.is_some_and(|e| e.ack() && e.playing() && e.rate == Some(1_000)),
        format!(
            "next write sends 'b' 1000 and the ring plays ({:?})",
            b.map(|e| e.code())
        ),
    );
    v.check(
        matches!(o4, Ok(WriteOutcome::WouldBlock)),
        format!("next write returns WouldBlock ({o4:?})"),
    );
    cx.proxy.inject(&[b"s".to_vec()], "cleanup: stop");
    cx.proxy.wait_replies(Duration::from_secs(2));
    let _ = tap.disconnect();
    cx.leave_idle(&mut v);
    Ok(v)
}

fn partial_room(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("partial_room");
    let cap = cx.bc.buffer_capacity as usize;
    let room = 50;
    let extra = 100;
    let mut last;
    {
        let mut raw = Raw::connect(cx.proxy.addr)?;
        let p = raw.cmd(b"p")?;
        v.check(p.response == b'a', "prepare ACKed");
        let fill = raw.cmd(&cmd::blank_data(cap - room))?;
        v.check(
            fill.response == b'a',
            format!("{}-point fill ACKed", cap - room),
        );
        let before = raw.cmd(b"?")?;
        let fb = before.dac_status.buffer_fullness;
        v.set("fullness_before", json!(fb));
        v.set("playback_before", json!(before.dac_status.playback_state));
        v.check(
            fb as usize == cap - room,
            format!("{room} points of room before the write (fullness {fb})"),
        );
        let d = raw.cmd(&cmd::blank_data(extra))?;
        v.set("data_reply", json!((d.response as char).to_string()));
        v.set(
            "fullness_in_data_reply",
            json!(d.dac_status.buffer_fullness),
        );
        let after = raw.cmd(b"?")?;
        let fa = after.dac_status.buffer_fullness;
        v.set("fullness_after", json!(fa));
        v.set("playback_after", json!(after.dac_status.playback_state));
        let behaviour = if fa as usize == cap && d.response == b'I' {
            "partial_write_then_nak_invalid"
        } else if fa == fb && d.response != b'a' {
            "whole_command_rejected"
        } else if d.response == b'a' {
            "acked"
        } else {
            "other"
        };
        v.set("behaviour", json!(behaviour));
        v.check(
            behaviour == "partial_write_then_nak_invalid",
            format!(
                "matches profile data_partial_write_then_nak_invalid=true \
                 ({behaviour}: {} with fullness {fb} -> {fa})",
                d.response as char
            ),
        );
        // The rest of a rejected payload must be consumed, not parsed as
        // commands; a desynced reply stream would show here.
        let in_sync = d.command == b'd' && after.command == b'?';
        v.check(
            in_sync,
            format!(
                "replies stay in sync ({} then {})",
                d.command as char, after.command as char
            ),
        );
        if !in_sync {
            v.notes
                .push("reply stream out of sync; dropped the connection".into());
            drop(raw);
            cx.leave_idle(&mut v);
            return Ok(v);
        }
        last = after.dac_status;
        for c in stop_cmds(&last) {
            last = raw.cmd(&c)?.dac_status;
        }
        v.check(
            last.playback_state == DacStatus::PLAYBACK_IDLE,
            "cleanup stopped the DAC",
        );
    }
    cx.leave_idle(&mut v);
    Ok(v)
}

fn clamp_guard(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("clamp_guard");
    // Stream API path: unsafe rates are refused before anything is sent.
    let conn_s = cx.proxy.state().conns;
    {
        let mut s = stream::connect_to(cx.proxy.addr, Some(&cx.bc), Duration::from_secs(2))
            .map_err(|e| format!("stream connect: {e:?}"))?;
        let max = cx.bc.max_point_rate;
        let tries: [(&str, Result<(), stream::CommunicationError>); 4] = [
            ("b 0", s.queue_commands().begin(0, 0).submit()),
            ("u 0", s.queue_commands().update(0, 0).submit()),
            ("q 0", s.queue_commands().point_rate(0).submit()),
            (
                "b over max",
                s.queue_commands().begin(0, max + 100_000).submit(),
            ),
        ];
        let mut outcomes = serde_json::Map::new();
        for (what, r) in tries {
            let invalid = matches!(
                &r,
                Err(stream::CommunicationError::Io(e)) if e.kind() == ErrorKind::InvalidInput
            );
            v.check(
                invalid,
                format!("stream API refuses {what} with InvalidInput"),
            );
            outcomes.insert(what.into(), json!(format!("{r:?}")));
        }
        v.set("stream_api_outcomes", Value::Object(outcomes));
    }
    cx.proxy.wait_quiet(Duration::from_secs(2));
    let leaked: Vec<String> = cx
        .events()
        .iter()
        .filter(|e| e.conn > conn_s && e.rate.is_some())
        .map(|e| format!("{} {:?} {:?}", op_name(e.op), e.rate, e.kind))
        .collect();
    v.check(
        leaked.is_empty(),
        format!("stream API put no rate command on the wire ({leaked:?})"),
    );

    // Backend path: out-of-range pps is clamped.
    let conn0 = cx.proxy.state().conns;
    let (mut tap, _) = cx.tap();
    tap.connect().map_err(|e| e.to_string())?;
    let o1 = tap.try_write_points(0, &blanks(1200));
    v.set("pps0_outcome", json!(format!("{o1:?}")));
    let o2 = tap.try_write_points(200_000, &blanks(100));
    v.set("pps200000_outcome", json!(format!("{o2:?}")));
    let _ = tap.disconnect();
    cx.proxy.wait_quiet(Duration::from_secs(3));
    let rates: Vec<Value> = cx
        .events()
        .iter()
        .filter(|e| e.conn > conn0 && e.rate.is_some())
        .map(|e| json!({"op": op_name(e.op), "rate": e.rate, "sent_to_dac": e.on_wire()}))
        .collect();
    v.set("rates_on_backend_socket", json!(rates));
    let all: Vec<u32> = rates
        .iter()
        .filter_map(|r| r["rate"].as_u64())
        .map(|r| r as u32)
        .collect();
    v.check(!all.is_empty(), "the backend put a rate on the wire");
    v.check(
        all.iter().all(|r| (1..=cx.bc.max_point_rate).contains(r)),
        format!("every rate within 1..={} ({all:?})", cx.bc.max_point_rate),
    );
    v.check(
        all.first() == Some(&(cx.bc.max_point_rate / 16)),
        "pps 0 became max/16",
    );
    v.check(
        all.contains(&cx.bc.max_point_rate),
        "pps 200000 became the advertised max",
    );
    cx.leave_idle(&mut v);
    Ok(v)
}

fn steady_stream(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("steady_stream_60s");
    let conn0 = cx.proxy.state().conns;
    let t0 = cx
        .proxy
        .rec
        .lock()
        .unwrap()
        .as_ref()
        .map_or_else(Instant::now, |r| r.t0);
    let (tap, _) = cx.tap();
    let secs = cx.steady;
    let mut cfg = StreamConfig::new(cx.steady_pps);
    if let Some(t) = cx.steady_target {
        cfg = cfg.with_target_buffer(t);
    }
    v.set("pps", json!(cx.steady_pps));
    v.set("target_ms", json!(cfg.target_buffer().as_millis() as u64));
    let run = cx.stream_cfg(tap, cfg, move |c| {
        thread::sleep(secs);
        let _ = c.stop();
    })?;
    v.set("seconds", json!(secs.as_secs()));
    let evs = cx.events();
    let first_play = evs.iter().position(|e| e.playing()).ok_or("never played")?;
    let last_stop = evs[first_play..]
        .iter()
        .rposition(|e| e.op == b's')
        .map_or(evs.len(), |i| first_play + i);
    let window = &evs[first_play..last_stop];
    let underflows = window
        .iter()
        .filter(|e| {
            e.idle()
                || e.st()
                    .is_some_and(|s| s.playback_flags & PBF_UNDERFLOW != 0)
        })
        .count();
    v.check(
        underflows == 0,
        format!("no underflow ({underflows} idle replies)"),
    );
    let prepares = evs.iter().filter(|e| e.op == b'p').count();
    v.check(prepares == 1, format!("one prepare ({prepares})"));
    let hellos = evs
        .iter()
        .filter(|e| e.kind == Kind::Hello && e.conn > conn0)
        .count();
    v.check(hellos == 1, format!("no reconnect ({hellos} connections)"));
    let full: Vec<i64> = window
        .iter()
        .filter(|e| e.op == b'd' && e.playing())
        .filter_map(|e| e.st().map(|s| s.buffer_fullness as i64))
        .collect();
    v.set("dac_fullness_after_write", stats(&full));
    let est: Vec<i64> = run
        .log
        .iter()
        .filter(|w| matches!(w.outcome, Ok(WriteOutcome::Written)))
        .map(|w| w.est_before as i64)
        .collect();
    v.set("estimate_before_write", stats(&est));
    let err = est_error_stats(&run.log, None);
    v.set("estimate_error", err);
    // A DAC round trip that stalls (seen on hardware: 20-35 ms) makes the
    // status it returns stale, or lands the write later than estimated.
    // That is transport latency, not estimator drift, so judge the estimator
    // on writes where neither this call nor the previous one was slow.
    let slow = Duration::from_millis(10);
    let calm: Vec<WriteLog> = run
        .log
        .windows(2)
        .filter(|w| w[0].dur <= slow && w[1].dur <= slow)
        .map(|w| w[1].clone())
        .collect();
    let calm_err = est_error_stats(&calm, None);
    v.set(
        "estimate_error_excluding_slow_round_trips",
        calm_err.clone(),
    );
    v.check(
        max_abs(&calm_err).is_some_and(|m| m <= 300),
        "estimate within 300 points of the DAC, excluding slow round trips",
    );
    let slow_calls: Vec<Value> = run
        .log
        .iter()
        .filter(|w| w.dur > slow)
        .map(|w| json!({"at_ms": ms_between(t0, w.t), "ms": w.dur.as_secs_f64() * 1e3}))
        .collect();
    v.set("slow_round_trips", json!(slow_calls));
    let min_full = full.iter().min().copied().unwrap_or(0);
    v.check(
        min_full > 0,
        format!("DAC ring never empty after a write (min {min_full})"),
    );
    let writes = run
        .log
        .iter()
        .filter(|w| matches!(w.outcome, Ok(WriteOutcome::Written)))
        .count();
    v.set("writes", json!(writes));
    v.set("host_timing", host_timing(cx, &run.log, t0));
    let underflow_at: Vec<f64> = window
        .iter()
        .filter(|e| e.idle())
        .map(|e| ms_between(t0, e.t))
        .collect();
    v.set("underflow_seen_at_ms", json!(underflow_at));
    v.check(
        run.errors.is_empty(),
        format!("no stream errors {:?}", run.errors),
    );
    v.set("exit", json!(run.exit));
    cx.leave_idle(&mut v);
    Ok(v)
}

fn session_stop(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("session_stop");
    let (tap, _) = cx.tap();
    let run = cx.stream(tap, 10_000, |c| {
        thread::sleep(Duration::from_secs(2));
        let _ = c.stop();
    })?;
    let evs = cx.events();
    let begin = evs
        .iter()
        .position(|e| e.op == b'b' && e.kind == Kind::Forward)
        .ok_or("no begin")?;
    let stops: Vec<&Ev> = evs[begin..]
        .iter()
        .filter(|e| e.op == b's' && e.on_wire())
        .collect();
    v.set(
        "stops_after_begin",
        json!(stops
            .iter()
            .map(|e| format!(
                "{:?} {} pb {:?}",
                e.kind,
                e.code(),
                e.st().map(|s| s.playback_state)
            ))
            .collect::<Vec<_>>()),
    );
    v.check(
        stops.len() == 1 && stops[0].kind == Kind::Forward,
        format!(
            "exactly one 's' from the backend after 'b' ({})",
            stops.len()
        ),
    );
    v.check(
        stops.first().is_some_and(|e| e.ack() && e.idle()),
        "the 's' is ACKed and the DAC is idle",
    );
    v.check(
        run.errors.is_empty(),
        format!("no stream errors {:?}", run.errors),
    );
    v.set("exit", json!(run.exit));
    let conn = cx.proxy.state().conns;
    cx.leave_idle(&mut v);
    let next_hello = cx
        .events()
        .into_iter()
        .find(|e| e.conn > conn && e.kind == Kind::Hello);
    let hello_pb = next_hello
        .as_ref()
        .and_then(|e| e.st().map(|s| s.playback_state));
    v.set("next_hello_playback", json!(hello_pb));
    v.check(
        hello_pb == Some(DacStatus::PLAYBACK_IDLE),
        format!("the next hello shows Idle ({hello_pb:?})"),
    );
    Ok(v)
}

fn broadcast_discovery(cx: &Ctx) -> Result<Verdict, String> {
    let mut v = Verdict::new("broadcast_discovery");
    if cx.sim {
        v.pass = None;
        v.notes.push("skipped in simulator mode".into());
        return Ok(v);
    }
    let ip = cx.proxy.upstream.ip();
    // Record one broadcast, MAC device half zeroed.
    if let Some(mut b) = hear_broadcast(ip, Duration::from_secs(3)) {
        b.mac_address[3..].fill(0);
        let mut bytes = Vec::new();
        bytes.write_bytes(b).map_err(|e| e.to_string())?;
        if let Some(rec) = cx.proxy.rec.lock().unwrap().as_mut() {
            rec.event(
                "broadcast",
                &[],
                0,
                &bytes,
                "ok",
                Instant::now(),
                Instant::now(),
                "UDP broadcast on port 7654, re-encoded from the parsed frame, MAC device half zeroed",
            );
        }
    }
    let mut enabled = EnabledDacTypes::none();
    enabled.enable(DacType::EtherDream);
    let mut disc = DacDiscovery::new(enabled);
    let t = Instant::now();
    let found = disc
        .scan()
        .into_iter()
        .find(|d| d.info().ip_address == Some(ip));
    v.set("scan_ms", json!(ms_between(t, Instant::now())));
    let Some(dev) = found else {
        v.check(false, format!("EtherDreamDiscoverer found {ip}"));
        return Ok(v);
    };
    v.check(true, format!("EtherDreamDiscoverer found {ip}"));
    let scan_caps = dev.caps().clone();
    v.set(
        "scan_time_caps",
        json!({"max_points_per_chunk": scan_caps.max_points_per_chunk, "pps_max": scan_caps.pps_max}),
    );
    // Constructing the backend does not open a TCP connection.
    let backend = disc.connect(dev).map_err(|e| e.to_string())?;
    let caps = backend.caps().clone();
    v.set(
        "backend_caps",
        json!({"max_points_per_chunk": caps.max_points_per_chunk, "pps_max": caps.pps_max}),
    );
    let (want_points, want_pps) = (cx.bc.buffer_capacity as usize, cx.bc.max_point_rate);
    v.check(
        caps.max_points_per_chunk == want_points && caps.pps_max == want_pps,
        format!("backend capabilities are the advertised {want_points} points and {want_pps} pps"),
    );
    if scan_caps.max_points_per_chunk != caps.max_points_per_chunk {
        v.notes.push(format!(
            "note: DiscoveredDevice::caps() (what list_devices reports) says {} points, \
             not the advertised {}",
            scan_caps.max_points_per_chunk, caps.max_points_per_chunk
        ));
    }
    Ok(v)
}

/// Whether a block by the interlock is one the scenario provokes on purpose.
/// Only `clamp_guard` does: the backend clamps pps 200 000 to an `'u'` at the
/// advertised maximum, which is above the forwarded range.
fn expected_block(scenario: &str, e: &Ev, bc: &DacBroadcast) -> bool {
    scenario == "clamp_guard"
        && e.op == b'u'
        && e.rate == Some(bc.max_point_rate)
        && !SAFE_RATES.contains(&bc.max_point_rate)
}

fn hear_broadcast(ip: std::net::IpAddr, wait: Duration) -> Option<DacBroadcast> {
    let mut rx = recv_dac_broadcasts().ok()?;
    let _ = rx.set_timeout(Some(Duration::from_millis(300)));
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if let Ok((b, src)) = rx.next_broadcast() {
            if src.ip() == ip {
                return Some(b);
            }
        }
    }
    None
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let sim = match &args.sim {
        Some(name) => {
            let profile = FirmwareProfile::all()
                .into_iter()
                .find(|p| p.name == name.as_str())
                .unwrap_or_else(|| panic!("unknown profile {name}"));
            Some(SimServer::loopback(profile)?)
        }
        None => None,
    };
    let (upstream, bc) = match (&sim, args.addr) {
        (Some(s), _) => (s.addr(), s.broadcast()),
        (None, Some(a)) => match hear_broadcast(a.ip(), Duration::from_secs(4)) {
            Some(b) => (a, b),
            None => {
                eprintln!("no broadcast from {}; refusing to guess capacity", a.ip());
                std::process::exit(2);
            }
        },
        (None, None) => {
            eprintln!("pass --addr or --sim");
            std::process::exit(2);
        }
    };
    eprintln!(
        "DAC {upstream}: capacity {} max rate {} (broadcast playback state {})",
        bc.buffer_capacity, bc.max_point_rate, bc.dac_status.playback_state
    );
    if bc.dac_status.playback_state != DacStatus::PLAYBACK_IDLE {
        eprintln!("broadcast shows the DAC is not idle; another client is streaming");
        std::process::exit(2);
    }
    // The DAC NAKs a rate above its maximum after consuming only the opcode
    // and parses the argument bytes as commands, so every rate the proxy
    // forwards must be within the advertised maximum.
    if bc.max_point_rate < *SAFE_RATES.end() {
        eprintln!(
            "DAC advertises max rate {}, below the top of the forwarded range {SAFE_RATES:?}; \
             refusing to run",
            bc.max_point_rate
        );
        std::process::exit(2);
    }
    std::fs::create_dir_all(&args.out)?;
    let proxy = Proxy::start(upstream, sim.is_none())?;
    let cx = Ctx {
        proxy,
        bc,
        sim: sim.is_some(),
        out: args.out.clone(),
        prefix: args.prefix.clone(),
        steady: Duration::from_secs(args.steady_secs),
        steady_pps: args.steady_pps,
        steady_target: args.steady_target_ms.map(Duration::from_millis),
    };
    let names: Vec<String> = if args.scenarios.is_empty() {
        ALL_SCENARIOS.iter().map(|s| s.to_string()).collect()
    } else {
        args.scenarios.clone()
    };
    let mut summary = BufWriter::new(File::create(
        args.out.join(format!("{}_summary.jsonl", args.prefix)),
    )?);
    let mut verdicts = Vec::new();
    for name in &names {
        if let Some(why) = cx.proxy.state().aborted.clone() {
            eprintln!("stopping before {name}: {why}");
            break;
        }
        eprintln!("== {name}");
        cx.begin(name)?;
        let result = match name.as_str() {
            "stale_full_reconnect" => stale_full_reconnect(&cx),
            "underflow_recovery" => underflow_recovery(&cx),
            "estop_recovery" => estop_recovery(&cx),
            "rate_change" => rate_change(&cx),
            "full_capacity_chunk" => full_capacity_chunk(&cx),
            "clamp_guard" => clamp_guard(&cx),
            "steady_stream_60s" => steady_stream(&cx),
            "session_stop" => session_stop(&cx),
            "partial_room" => partial_room(&cx),
            "broadcast_discovery" => broadcast_discovery(&cx),
            other => Err(format!("unknown scenario {other}")),
        };
        cx.finish();
        let mut v = result.unwrap_or_else(|e| {
            let mut v = Verdict::new(name);
            v.check(false, e);
            v
        });
        if let Some(why) = cx.proxy.state().aborted.clone() {
            v.check(false, format!("aborted: {why}"));
        }
        let blocked: Vec<String> = cx
            .events()
            .iter()
            .filter(|e| e.kind == Kind::Blocked && !expected_block(name, e, &cx.bc))
            .map(|e| format!("{} {:?}", op_name(e.op), e.rate))
            .collect();
        if !blocked.is_empty() {
            v.check(false, format!("interlock refused {blocked:?}"));
        }
        let verdict = match v.pass {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "SKIP",
        };
        eprintln!("   {verdict} {}", v.metrics);
        for n in &v.notes {
            eprintln!("   {n}");
        }
        writeln!(
            summary,
            "{}",
            json!({"scenario": v.name, "verdict": verdict, "metrics": v.metrics, "notes": v.notes})
        )?;
        summary.flush()?;
        verdicts.push((v.name.clone(), verdict));
    }
    println!();
    for (n, v) in &verdicts {
        println!("{v:4}  {n}");
    }
    if verdicts.iter().any(|(_, v)| *v == "FAIL") {
        std::process::exit(1);
    }
    Ok(())
}
