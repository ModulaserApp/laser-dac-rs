//! Capture Ether Dream wire traces as JSONL fixtures.
//!
//! Connects to a DAC over TCP, runs a fixed set of probe scenarios and writes
//! one JSONL file per scenario. Every event records the command bytes sent,
//! the raw reply, and timing, in the format the replay tests in
//! `src/protocols/ether_dream/replay.rs` read. See
//! `tests/fixtures/ether_dream/README.md` for the field list.
//!
//! SAFETY: this tool only ever sends blanked points (every field zero: no
//! colour, no intensity, centred), so it is safe with a laser attached. Every
//! `begin`, `update` and `queue_rate` goes through [`Probe::rate_ok`], which
//! refuses a rate of 0 (hangs ED2 firmware until power-cycled) or one above
//! the advertised maximum (NAKed after consuming only the opcode, so the
//! argument bytes are then parsed as further commands). It only sends command
//! sequences already probed on ED2 hardware.
//!
//! Every connection checks the DAC's hello first and aborts unless playback
//! is idle and the light engine is Ready. Ether Dream 2 shares one playback state between all TCP clients,
//! so another application streaming to the same DAC would be interrupted and
//! would contaminate the capture. Close other clients before capturing.
//!
//! ```text
//! # Against a real DAC (its UDP broadcast must be heard; the capacity and
//! # maximum rate come from it):
//! cargo run --example ether_dream_capture --features testutils,ether-dream -- \
//!     --addr 192.168.254.66:7765 --out tests/fixtures/ether_dream --prefix ed2_r331_cap
//!
//! # Self-test against the in-process simulator:
//! cargo run --example ether_dream_capture --features testutils,ether-dream -- \
//!     --sim ed2-r331 --out /tmp/ed-capture
//! ```

use std::fs::File;
use std::io::{self, BufWriter, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use laser_dac::protocols::ether_dream::protocol::{
    DacBroadcast, DacResponse, DacStatus, ReadBytes, SizeBytes, WriteBytes,
};
use laser_dac::protocols::ether_dream::sim::model::cmd;
use laser_dac::protocols::ether_dream::sim::SimServer;
use laser_dac::protocols::ether_dream::{recv_dac_broadcasts, FirmwareProfile};

/// Only rates from this list are ever sent.
const RATE_FAST: u32 = 30_000;
const RATE_MID: u32 = 20_000;
const RATE_QUEUED: u32 = 25_000;
const RATE_SLOW: u32 = 1_000;

const READ_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Parser)]
#[command(about = "Capture Ether Dream protocol traces (blanked points only)")]
struct Args {
    /// DAC TCP address, e.g. 192.168.254.66:7765.
    #[arg(long, conflicts_with = "sim")]
    addr: Option<SocketAddr>,
    /// Capture against the simulator with this firmware profile name instead.
    #[arg(long)]
    sim: Option<String>,
    /// Output directory.
    #[arg(long, default_value = ".")]
    out: PathBuf,
    /// File name prefix; files are `<prefix>_<scenario>.jsonl`.
    #[arg(long, default_value = "capture")]
    prefix: String,
    /// Also send an unknown command last. ED2 resets the TCP connection.
    #[arg(long)]
    include_reset: bool,
    /// Only listen for the DAC's UDP broadcast and record it. Opens no TCP
    /// connection and sends nothing.
    #[arg(long, conflicts_with = "sim")]
    listen_only: bool,
}

/// Wire facts learned before probing.
struct Limits {
    max_rate: u32,
    capacity: usize,
}

struct Recorder {
    out: BufWriter<File>,
    phase: &'static str,
    seq: u32,
    t0: Instant,
    last: Duration,
}

impl Recorder {
    fn new(path: &Path, phase: &'static str) -> io::Result<Self> {
        Ok(Self {
            out: BufWriter::new(File::create(path)?),
            phase,
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
        note: &str,
    ) -> io::Result<()> {
        self.seq += 1;
        let t = start.saturating_duration_since(self.t0);
        let end = Instant::now().saturating_duration_since(self.t0);
        let line = serde_json::json!({
            "seq": self.seq,
            "phase": self.phase,
            "op": op,
            "req": hex(req),
            "resp": hex(resp),
            "status": status,
            "n": n,
            "dt_us": t.saturating_sub(self.last).as_micros() as u64,
            "dur_us": (end - t).as_micros() as u64,
            "t_us": end.as_micros() as u64,
            "note": note,
        });
        self.last = end;
        writeln!(self.out, "{line}")
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// One TCP connection to the DAC, recording every exchange.
struct Probe<'a> {
    sock: TcpStream,
    rec: &'a mut Recorder,
    limits: &'a Limits,
    /// The connection is closed or out of sync; nothing more is sent on it.
    reset: bool,
}

impl<'a> Probe<'a> {
    fn connect(addr: SocketAddr, rec: &'a mut Recorder, limits: &'a Limits) -> io::Result<Self> {
        let sock = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        sock.set_nodelay(true)?;
        sock.set_read_timeout(Some(READ_TIMEOUT))?;
        let mut p = Self {
            sock,
            rec,
            limits,
            reset: false,
        };
        let start = Instant::now();
        let (resp, status) = p.read_reply(DacResponse::SIZE_BYTES);
        p.rec.event("hello", &[], 0, &resp, status, start, "")?;
        // Ether Dream 2 accepts a second client and shares one playback state
        // between connections, so probing a DAC that another application is
        // streaming to would interrupt that stream and mix its points into
        // the capture. Refuse unless the DAC is idle and its light engine is
        // Ready (not in e-stop, warm-up or cool-down).
        let hello = (status == "ok")
            .then(|| (&resp[..]).read_bytes::<DacResponse>().ok())
            .flatten();
        match hello {
            Some(r)
                if r.dac_status.playback_state == DacStatus::PLAYBACK_IDLE
                    && r.dac_status.light_engine_state == DacStatus::LIGHT_ENGINE_READY =>
            {
                Ok(p)
            }
            Some(r) if r.dac_status.playback_state != DacStatus::PLAYBACK_IDLE => {
                Err(io::Error::other(format!(
                    "DAC is busy (playback state {}, rate {}): another client is probably \
                     connected; disconnect it and retry",
                    r.dac_status.playback_state, r.dac_status.point_rate
                )))
            }
            Some(r) => Err(io::Error::other(format!(
                "DAC light engine is not Ready (state {}, flags 0x{:x}); clear the e-stop \
                 or wait for it to settle and retry",
                r.dac_status.light_engine_state, r.dac_status.light_engine_flags
            ))),
            None => Err(io::Error::other(format!("no valid hello ({status})"))),
        }
    }

    fn read_reply(&mut self, len: usize) -> (Vec<u8>, &'static str) {
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            match self.sock.read(&mut buf[got..]) {
                Ok(0) => {
                    self.reset = true;
                    return (buf[..got].to_vec(), "closed");
                }
                Ok(n) => got += n,
                // A late reply would be read as the next command's, so stop
                // using the connection.
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    self.reset = true;
                    return (buf[..got].to_vec(), "timeout");
                }
                Err(e) if e.kind() == ErrorKind::ConnectionReset => {
                    self.reset = true;
                    return (buf[..got].to_vec(), "reset");
                }
                Err(_) => {
                    self.reset = true;
                    return (buf[..got].to_vec(), "io");
                }
            }
        }
        (buf, "ok")
    }

    /// Send one command and record its 22-byte reply. Blanked data payloads
    /// are elided from `req` (see the fixture README).
    fn send(&mut self, op: &str, bytes: &[u8], note: &str) -> io::Result<Option<DacResponse>> {
        if self.reset {
            return Ok(None);
        }
        let start = Instant::now();
        self.sock.write_all(bytes)?;
        let (resp, status) = self.read_reply(DacResponse::SIZE_BYTES);
        let req = if bytes[0] == b'd' { &bytes[..3] } else { bytes };
        let mut note = note.to_string();
        if bytes[0] == b'd' {
            let n = u16::from_le_bytes([bytes[1], bytes[2]]);
            let extra = format!("payload elided: {n} blanked points, 18 zero bytes each");
            note = if note.is_empty() {
                extra
            } else {
                format!("{note}; {extra}")
            };
        }
        self.rec
            .event(op, req, bytes.len(), &resp, status, start, &note)?;
        if status != "ok" {
            return Ok(None);
        }
        Ok((&resp[..]).read_bytes::<DacResponse>().ok())
    }

    fn ping(&mut self, note: &str) -> io::Result<Option<DacResponse>> {
        self.send("ping", b"?", note)
    }

    fn data(&mut self, n: usize, note: &str) -> io::Result<Option<DacResponse>> {
        // Blanked points only.
        self.send("data", &cmd::blank_data(n), note)
    }

    fn rate_ok(&self, rate: u32) -> u32 {
        assert!(rate > 0, "a point rate of 0 hangs ED2 firmware");
        assert!(
            rate <= self.limits.max_rate,
            "rate above the advertised max"
        );
        rate
    }

    fn begin(&mut self, rate: u32, note: &str) -> io::Result<Option<DacResponse>> {
        let rate = self.rate_ok(rate);
        self.send("begin", &cmd::begin(rate), note)
    }

    fn version(&mut self) -> io::Result<()> {
        let start = Instant::now();
        self.sock.write_all(b"v")?;
        let (mut resp, status) = self.read_reply(DacResponse::SIZE_BYTES);
        let mut note = "";
        if status == "ok" && !(resp[0] != DacResponse::ACK && resp[1] == b'v') {
            let (rest, st2) = self.read_reply(32 - DacResponse::SIZE_BYTES);
            resp.extend_from_slice(&rest);
            if st2 != "ok" {
                note = "short version reply";
            }
        } else if status == "ok" {
            note = "version NAKed";
        }
        self.rec
            .event("version", b"v", 1, &resp, status, start, note)
    }
}

fn pause(d: Duration) {
    thread::sleep(d);
}

fn identity(addr: SocketAddr, rec: &mut Recorder, l: &Limits) -> io::Result<()> {
    let mut p = Probe::connect(addr, rec, l)?;
    p.ping("")?;
    p.version()?;
    p.ping("connection still in sync after 'v'")?;
    Ok(())
}

fn lifecycle(addr: SocketAddr, rec: &mut Recorder, l: &Limits, reset: bool) -> io::Result<()> {
    let mut p = Probe::connect(addr, rec, l)?;
    p.send("prepare", b"p", "")?;
    p.data(500, "")?;
    p.begin(RATE_FAST, "")?;
    // 500 points at 30 kpps last 16.7 ms; sample the drain until underflow.
    for _ in 0..6 {
        pause(Duration::from_millis(4));
        p.ping("")?;
    }
    p.data(100, "after underflow")?;
    p.begin(RATE_FAST, "begin while idle")?;
    p.send("prepare", b"p", "")?;
    p.data(300, "")?;
    p.begin(RATE_FAST, "")?;
    p.send("update", &cmd::update(p.rate_ok(RATE_MID)), "")?;
    p.send("point_rate", &cmd::queue_rate(p.rate_ok(RATE_QUEUED)), "")?;
    p.send("stop", b"s", "")?;
    p.send("stop", b"s", "stop while idle")?;
    p.data(10, "data while idle")?;
    p.send("estop", &[0x00], "")?;
    p.ping("")?;
    // Data during e-stop is deliberately not sent: it was never probed on
    // hardware (see the fixtures README, "Untested").
    p.send("clear_estop", b"c", "")?;
    p.ping("")?;
    p.send("prepare", b"p", "clears the sticky flags")?;
    p.send("stop", b"s", "")?;
    if reset {
        p.send("unknown", b"z", "unknown command")?;
    }
    Ok(())
}

fn capacity(addr: SocketAddr, rec: &mut Recorder, l: &Limits) -> io::Result<()> {
    let mut p = Probe::connect(addr, rec, l)?;
    p.send("prepare", b"p", "")?;
    let mut sent = 0usize;
    // Fill to the advertised capacity, then keep going in small steps until
    // the DAC refuses or holds twice what it advertises.
    let first = l.capacity.min(1000);
    for chunk in [first, l.capacity - first] {
        if chunk == 0 {
            continue;
        }
        p.data(chunk, "")?;
        sent += chunk;
    }
    while sent < l.capacity * 2 + 1 {
        match p.data(100, "beyond advertised capacity")? {
            Some(r) if r.response == DacResponse::ACK => sent += 100,
            _ => break,
        }
    }
    p.ping("")?;
    p.begin(RATE_SLOW, "")?;
    pause(Duration::from_millis(50));
    p.ping("50 ms at 1 kpps")?;
    p.send("stop", b"s", "")?;
    Ok(())
}

fn drain(addr: SocketAddr, rec: &mut Recorder, l: &Limits) -> io::Result<()> {
    let mut p = Probe::connect(addr, rec, l)?;
    p.send("prepare", b"p", "")?;
    p.data(1500, "")?;
    p.begin(RATE_FAST, "")?;
    // 1500 points at 30 kpps last 50 ms.
    for _ in 0..40 {
        pause(Duration::from_millis(2));
        match p.ping("")? {
            Some(r) if r.dac_status.playback_state == 0 => break,
            Some(_) => {}
            None => break,
        }
    }
    // Refill and stream for a while, as the backend would.
    p.send("prepare", b"p", "")?;
    p.data(900, "")?;
    p.begin(RATE_FAST, "")?;
    for _ in 0..20 {
        pause(Duration::from_millis(10));
        p.data(300, "")?;
    }
    p.send("stop", b"s", "")?;
    Ok(())
}

/// Listen for the broadcast from `ip` for up to `wait`.
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

/// Advertised limits, from the simulator or the DAC's broadcast. Exits if no
/// broadcast is heard: guessed limits would make the rate guard meaningless.
fn limits_for(addr: SocketAddr, sim: Option<&SimServer>) -> (Limits, DacBroadcast) {
    let heard = match sim {
        Some(s) => Some(s.broadcast()),
        None => hear_broadcast(addr.ip(), Duration::from_secs(3)),
    };
    let Some(b) = heard else {
        eprintln!(
            "no broadcast heard from {}; refusing to guess the capacity and maximum rate",
            addr.ip()
        );
        std::process::exit(2);
    };
    eprintln!(
        "broadcast: capacity {} max rate {} hw {} sw {}",
        b.buffer_capacity, b.max_point_rate, b.hw_revision, b.sw_revision
    );
    let limits = Limits {
        max_rate: b.max_point_rate,
        capacity: b.buffer_capacity as usize,
    };
    (limits, b)
}

/// Record a broadcast as a single event with its raw 36 bytes in `resp`.
fn record_broadcast(path: &Path, b: &DacBroadcast) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(DacBroadcast::SIZE_BYTES);
    bytes.write_bytes(b)?;
    let mut rec = Recorder::new(path, "broadcast")?;
    rec.event(
        "broadcast",
        &[],
        0,
        &bytes,
        "ok",
        Instant::now(),
        "UDP broadcast on port 7654, re-encoded from the parsed frame",
    )
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
    let addr = match (&sim, args.addr) {
        (Some(s), _) => s.addr(),
        (None, Some(a)) => a,
        (None, None) => {
            eprintln!("pass --addr or --sim");
            std::process::exit(2);
        }
    };
    let (limits, heard) = limits_for(addr, sim.as_ref());
    std::fs::create_dir_all(&args.out)?;
    record_broadcast(
        &args.out.join(format!("{}_broadcast.jsonl", args.prefix)),
        &heard,
    )?;
    if args.listen_only {
        return Ok(());
    }
    // Pre-flight rate guard: refuse to connect at all if any rate this tool
    // can send is 0 or above the advertised maximum.
    for rate in [RATE_FAST, RATE_MID, RATE_QUEUED, RATE_SLOW] {
        if rate == 0 || rate > limits.max_rate {
            eprintln!(
                "refusing to run: rate {rate} outside 1..={}",
                limits.max_rate
            );
            std::process::exit(2);
        }
    }
    let path = |s: &str| args.out.join(format!("{}_{s}.jsonl", args.prefix));

    identity(
        addr,
        &mut Recorder::new(&path("identity"), "identity")?,
        &limits,
    )?;
    lifecycle(
        addr,
        &mut Recorder::new(&path("lifecycle"), "lifecycle")?,
        &limits,
        args.include_reset,
    )?;
    capacity(
        addr,
        &mut Recorder::new(&path("capacity"), "capacity")?,
        &limits,
    )?;
    drain(addr, &mut Recorder::new(&path("drain"), "drain")?, &limits)?;
    eprintln!("wrote {}", args.out.display());
    Ok(())
}
