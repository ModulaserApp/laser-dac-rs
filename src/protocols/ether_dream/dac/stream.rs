//! Ether Dream DAC streaming interface.

use super::{Addressed, ProtocolError};
use crate::protocols::ether_dream::protocol::{
    self, Command, ReadBytes, SizeBytes, WriteBytes, WriteToBytes,
};
use std::borrow::Cow;
use std::error::Error;
use std::io::{self, BufReader, Read, Write};
use std::{fmt, mem, net, ops, time};

/// A bi-directional communication stream between the user and a `Dac`.
pub struct Stream {
    dac: Addressed,
    tcp_reader: BufReader<net::TcpStream>,
    tcp_writer: net::TcpStream,
    command_buffer: Vec<QueuedCommand>,
    point_buffer: Vec<protocol::DacPoint>,
    bytes: Vec<u8>,
    /// When the command whose reply carried the current status was sent.
    status_sent_at: time::Instant,
    /// When the reply that carried the current status was read.
    status_received_at: time::Instant,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum QueuedCommand {
    PrepareStream,
    Begin(protocol::command::Begin),
    Update(protocol::command::Update),
    PointRate(protocol::command::PointRate),
    Data(ops::Range<usize>),
    Stop,
    EmergencyStop,
    ClearEmergencyStop,
    Ping,
}

impl QueuedCommand {
    /// The point rate this command sends, if it carries one.
    fn point_rate(&self) -> Option<u32> {
        match self {
            QueuedCommand::Begin(b) => Some(b.point_rate),
            QueuedCommand::Update(u) => Some(u.point_rate),
            QueuedCommand::PointRate(r) => Some(r.0),
            _ => None,
        }
    }
}

pub struct CommandQueue<'a> {
    stream: &'a mut Stream,
}

#[derive(Debug)]
pub enum CommunicationError {
    Io(io::Error),
    Protocol(ProtocolError),
    Response(ResponseError),
}

#[derive(Debug)]
pub struct ResponseError {
    pub response: protocol::DacResponse,
    pub kind: ResponseErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResponseErrorKind {
    UnexpectedCommand(u8),
    Nak(Nak),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Nak {
    Full,
    Invalid,
    StopCondition,
}

impl Stream {
    fn send_command<C>(&mut self, command: C) -> io::Result<()>
    where
        C: Command + WriteToBytes,
    {
        send_command(&mut self.bytes, &mut self.tcp_writer, command)
    }

    fn recv_response(&mut self, expected_command: u8) -> Result<(), CommunicationError> {
        recv_response_buffered(
            &mut self.bytes,
            &mut self.tcp_reader,
            &mut self.dac,
            expected_command,
        )
    }

    pub fn dac(&self) -> &Addressed {
        &self.dac
    }

    /// When the command that produced the current [`dac`](Self::dac) status
    /// was sent; for the connection hello, when the connection was opened.
    ///
    /// The firmware samples its status while it processes a command, so this
    /// is a better anchor for decaying the reported fullness than the time
    /// the reply arrived. A delayed reply otherwise makes the ring look
    /// fuller than it is. Erring early is the conservative direction.
    pub fn status_sent_at(&self) -> time::Instant {
        self.status_sent_at
    }

    /// When the reply that produced the current [`dac`](Self::dac) status was
    /// read from the socket; for the connection hello, when it was read.
    ///
    /// The firmware sampled the status no later than this, so decaying the
    /// reported fullness from here never under-reads it. Use it where
    /// over-reading is the safe direction, such as deciding whether a chunk
    /// fits. [`status_sent_at`](Self::status_sent_at) is the matching lower
    /// bound.
    pub fn status_received_at(&self) -> time::Instant {
        self.status_received_at
    }

    /// Ask the firmware for its build string with the `'v'` command.
    ///
    /// Send this only when no other command is in flight. Firmware that does
    /// not implement `'v'` may reset the connection, which surfaces as an I/O
    /// error; the stream is unusable afterwards and must be reconnected.
    ///
    /// Returns `Ok(None)` if the firmware answered with a normal NAK frame
    /// instead of a build string.
    pub fn query_version(&mut self) -> Result<Option<String>, CommunicationError> {
        const V: u8 = protocol::command::Version::START_BYTE;
        let sent_at = time::Instant::now();
        self.send_command(protocol::command::Version)?;
        let mut raw = [0u8; protocol::command::Version::RESPONSE_SIZE_BYTES];
        let head = protocol::DacResponse::SIZE_BYTES;
        read_exact_with_budget(&mut self.tcp_reader, &mut raw[..head], VERSION_BUDGET)?;
        // A NAK frame for 'v' starts with a response code then 'v'. No real
        // build string starts that way.
        if Nak::from_protocol(raw[0]).is_some() && raw[1] == V {
            let response = (&raw[..head]).read_bytes::<protocol::DacResponse>()?;
            self.dac.update_status(&response.dac_status)?;
            self.status_sent_at = sent_at;
            self.status_received_at = time::Instant::now();
            return Ok(None);
        }
        read_exact_with_budget(&mut self.tcp_reader, &mut raw[head..], VERSION_BUDGET)?;
        Ok(Some(protocol::command::Version::decode_response(&raw)))
    }

    /// Address of the connected DAC.
    pub fn peer_addr(&self) -> io::Result<net::SocketAddr> {
        self.tcp_writer.peer_addr()
    }

    pub fn queue_commands(&mut self) -> CommandQueue<'_> {
        self.command_buffer.clear();
        self.point_buffer.clear();
        CommandQueue { stream: self }
    }

    pub fn set_nodelay(&self, b: bool) -> io::Result<()> {
        self.tcp_writer.set_nodelay(b)
    }

    pub fn nodelay(&self) -> io::Result<bool> {
        self.tcp_writer.nodelay()
    }

    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.tcp_writer.set_ttl(ttl)
    }

    pub fn ttl(&self) -> io::Result<u32> {
        self.tcp_writer.ttl()
    }

    pub fn set_read_timeout(&self, duration: Option<time::Duration>) -> io::Result<()> {
        self.tcp_reader.get_ref().set_read_timeout(duration)
    }

    pub fn set_write_timeout(&self, duration: Option<time::Duration>) -> io::Result<()> {
        self.tcp_writer.set_write_timeout(duration)
    }

    pub fn set_timeout(&self, duration: Option<time::Duration>) -> io::Result<()> {
        self.set_read_timeout(duration)?;
        self.set_write_timeout(duration)
    }
}

impl<'a> CommandQueue<'a> {
    pub fn prepare_stream(self) -> Self {
        self.stream
            .command_buffer
            .push(QueuedCommand::PrepareStream);
        self
    }

    pub fn begin(self, low_water_mark: u16, point_rate: u32) -> Self {
        let begin = protocol::command::Begin {
            low_water_mark,
            point_rate,
        };
        self.stream.command_buffer.push(QueuedCommand::Begin(begin));
        self
    }

    pub fn update(self, low_water_mark: u16, point_rate: u32) -> Self {
        let update = protocol::command::Update {
            low_water_mark,
            point_rate,
        };
        self.stream
            .command_buffer
            .push(QueuedCommand::Update(update));
        self
    }

    pub fn point_rate(self, point_rate: u32) -> Self {
        let point_rate = protocol::command::PointRate(point_rate);
        self.stream
            .command_buffer
            .push(QueuedCommand::PointRate(point_rate));
        self
    }

    pub fn data<I>(self, points: I) -> Self
    where
        I: IntoIterator<Item = protocol::DacPoint>,
    {
        let start = self.stream.point_buffer.len();
        self.stream.point_buffer.extend(points);
        let mut end = self.stream.point_buffer.len();
        // A single Data command addresses at most u16::MAX points. Drop any
        // excess rather than panicking on the output thread; the write path
        // enforces the same bound. In practice chunks are capped far below this.
        if end - start > u16::MAX as usize {
            log::warn!(
                "Ether Dream: data chunk of {} points exceeds {}, truncating",
                end - start,
                u16::MAX
            );
            end = start + u16::MAX as usize;
            self.stream.point_buffer.truncate(end);
        }
        self.stream
            .command_buffer
            .push(QueuedCommand::Data(start..end));
        self
    }

    pub fn stop(self) -> Self {
        self.stream.command_buffer.push(QueuedCommand::Stop);
        self
    }

    pub fn emergency_stop(self) -> Self {
        self.stream
            .command_buffer
            .push(QueuedCommand::EmergencyStop);
        self
    }

    pub fn clear_emergency_stop(self) -> Self {
        self.stream
            .command_buffer
            .push(QueuedCommand::ClearEmergencyStop);
        self
    }

    pub fn ping(self) -> Self {
        self.stream.command_buffer.push(QueuedCommand::Ping);
        self
    }

    /// Send every queued command, then read one response per command.
    ///
    /// Every response is read even after a NAK, and the first NAK is
    /// returned, so the stream stays in sync for the next submit.
    ///
    /// A `'b'`, `'u'` or `'q'` whose rate is 0, or above the DAC's advertised
    /// `max_point_rate` (when it advertises one), is rejected with an
    /// [`io::ErrorKind::InvalidInput`] error before anything is sent. Rate 0
    /// hangs Ether Dream firmware until a power cycle. An over-max rate is
    /// NAKed after only the opcode byte is consumed, and the firmware then
    /// parses the argument bytes as further commands (a low-water mark of 0
    /// becomes two emergency stops).
    pub fn submit(self) -> Result<(), CommunicationError> {
        let CommandQueue { stream } = self;

        let max_rate = stream.dac().max_point_rate;
        let unsafe_rate = |rate: u32| rate == 0 || (max_rate > 0 && rate > max_rate);
        if let Some(rate) = stream
            .command_buffer
            .iter()
            .filter_map(QueuedCommand::point_rate)
            .find(|&rate| unsafe_rate(rate))
        {
            stream.command_buffer.clear();
            stream.point_buffer.clear();
            return Err(CommunicationError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("point rate {rate} is outside 1..={max_rate} pps"),
            )));
        }

        let mut command_bytes = vec![];
        let mut command_buffer = mem::take(&mut stream.command_buffer);

        let mut sent_at = Vec::with_capacity(command_buffer.len());
        for command in command_buffer.drain(..) {
            sent_at.push(time::Instant::now());
            let start_byte = match command {
                QueuedCommand::PrepareStream => {
                    stream.send_command(protocol::command::PrepareStream)?;
                    protocol::command::PrepareStream::START_BYTE
                }
                QueuedCommand::Begin(begin) => {
                    stream.send_command(begin)?;
                    protocol::command::Begin::START_BYTE
                }
                QueuedCommand::Update(update) => {
                    stream.send_command(update)?;
                    protocol::command::Update::START_BYTE
                }
                QueuedCommand::PointRate(point_rate) => {
                    stream.send_command(point_rate)?;
                    protocol::command::PointRate::START_BYTE
                }
                QueuedCommand::Data(range) => {
                    let points = Cow::Borrowed(&stream.point_buffer[range]);
                    let data = protocol::command::Data { points };
                    send_command(&mut stream.bytes, &mut stream.tcp_writer, data)?;
                    protocol::command::Data::START_BYTE
                }
                QueuedCommand::Stop => {
                    stream.send_command(protocol::command::Stop)?;
                    protocol::command::Stop::START_BYTE
                }
                QueuedCommand::EmergencyStop => {
                    stream.send_command(protocol::command::EmergencyStop)?;
                    protocol::command::EmergencyStop::START_BYTE
                }
                QueuedCommand::ClearEmergencyStop => {
                    stream.send_command(protocol::command::ClearEmergencyStop)?;
                    protocol::command::ClearEmergencyStop::START_BYTE
                }
                QueuedCommand::Ping => {
                    stream.send_command(protocol::command::Ping)?;
                    protocol::command::Ping::START_BYTE
                }
            };
            command_bytes.push(start_byte);
        }

        mem::swap(&mut stream.command_buffer, &mut command_buffer);

        // Read every response even after a NAK. Returning at the first NAK
        // would leave the remaining responses queued in the socket, and the
        // next submit would read them as its own.
        let mut first_nak = None;
        for (command_byte, sent_at) in command_bytes.into_iter().zip(sent_at) {
            let result = stream.recv_response(command_byte);
            let received_at = time::Instant::now();
            match result {
                Ok(()) => {
                    stream.status_sent_at = sent_at;
                    stream.status_received_at = received_at;
                }
                Err(CommunicationError::Response(e))
                    if matches!(e.kind, ResponseErrorKind::Nak(_)) =>
                {
                    stream.status_sent_at = sent_at;
                    stream.status_received_at = received_at;
                    first_nak.get_or_insert(CommunicationError::Response(e));
                }
                Err(e) => return Err(e),
            }
        }

        match first_nak {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl protocol::DacResponse {
    fn check_errors(&self, expected_command: u8) -> Result<(), ResponseError> {
        if self.command != expected_command {
            return Err(ResponseError {
                response: *self,
                kind: ResponseErrorKind::UnexpectedCommand(self.command),
            });
        }

        if let Some(nak) = Nak::from_protocol(self.response) {
            return Err(ResponseError {
                response: *self,
                kind: ResponseErrorKind::Nak(nak),
            });
        }

        Ok(())
    }
}

impl Nak {
    pub fn from_protocol(nak: u8) -> Option<Self> {
        Some(match nak {
            protocol::DacResponse::NAK_FULL => Nak::Full,
            protocol::DacResponse::NAK_INVALID => Nak::Invalid,
            protocol::DacResponse::NAK_STOP_CONDITION => Nak::StopCondition,
            _ => return None,
        })
    }

    pub fn to_protocol(&self) -> u8 {
        match *self {
            Nak::Full => protocol::DacResponse::NAK_FULL,
            Nak::Invalid => protocol::DacResponse::NAK_INVALID,
            Nak::StopCondition => protocol::DacResponse::NAK_STOP_CONDITION,
        }
    }
}

impl Error for CommunicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CommunicationError::Io(err) => Some(err),
            CommunicationError::Protocol(err) => Some(err),
            CommunicationError::Response(err) => Some(err),
        }
    }
}

impl Error for ResponseError {}

impl fmt::Display for CommunicationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CommunicationError::Io(err) => err.fmt(f),
            CommunicationError::Protocol(err) => err.fmt(f),
            CommunicationError::Response(err) => err.fmt(f),
        }
    }
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.kind {
            ResponseErrorKind::UnexpectedCommand(_) => {
                write!(f, "received response to unexpected command")
            }
            ResponseErrorKind::Nak(nak) => match nak {
                Nak::Full => write!(f, "DAC responded with \"NAK - Full\""),
                Nak::Invalid => write!(f, "DAC responded with \"NAK - Invalid\""),
                Nak::StopCondition => write!(f, "DAC responded with \"NAK - Stop Condition\""),
            },
        }
    }
}

impl From<io::Error> for CommunicationError {
    fn from(err: io::Error) -> Self {
        CommunicationError::Io(err)
    }
}

impl From<ProtocolError> for CommunicationError {
    fn from(err: ProtocolError) -> Self {
        CommunicationError::Protocol(err)
    }
}

impl From<ResponseError> for CommunicationError {
    fn from(err: ResponseError) -> Self {
        CommunicationError::Response(err)
    }
}

/// Per-`read` timeout. A single response may span several of these if the DAC
/// is briefly slow; see [`READ_BUDGET`].
const READ_TIMEOUT: time::Duration = time::Duration::from_millis(500);

/// Total time budget for reading one complete response before giving up. A
/// Wi-Fi latency spike should be ridden out (retrying the in-progress read)
/// rather than restarting the whole stream.
const READ_BUDGET: time::Duration = time::Duration::from_secs(2);

/// Budget for the `'v'` reply. Kept short: firmware that ignores `'v'`
/// should not stall a connect for the full [`READ_BUDGET`].
const VERSION_BUDGET: time::Duration = time::Duration::from_millis(500);

/// Write timeout — bounds how long a hung DAC can wedge `write_all`.
const WRITE_TIMEOUT: time::Duration = time::Duration::from_secs(2);

/// Establishes a TCP stream connection with the DAC at the given address.
pub fn connect(
    broadcast: &protocol::DacBroadcast,
    dac_ip: net::IpAddr,
) -> Result<Stream, CommunicationError> {
    let addr = net::SocketAddr::new(dac_ip, protocol::COMMUNICATION_PORT);
    connect_inner(broadcast, addr, &net::TcpStream::connect)
}

/// Connect to a DAC by socket address, without needing its UDP broadcast.
///
/// Use this when broadcasts cannot reach the host (different subnet, a
/// firewall, or link-local setups where they are simply not seen) or to reach
/// a DAC on a non-standard port, such as a simulator.
///
/// When `broadcast` is `None` the DAC's identity is synthesised: the MAC
/// address and revisions are reported as zero, and the buffer capacity and
/// maximum point rate fall back to 1799 points, the smallest known ring
/// (ED1), and 100 000 pps, which every known Ether Dream advertises. The
/// live status comes from the hello
/// frame the DAC sends on connect either way.
pub fn connect_to(
    addr: net::SocketAddr,
    broadcast: Option<&protocol::DacBroadcast>,
    timeout: time::Duration,
) -> Result<Stream, CommunicationError> {
    let fallback;
    let broadcast = match broadcast {
        Some(b) => b,
        None => {
            fallback = synthetic_broadcast();
            &fallback
        }
    };
    let connect = |addr| net::TcpStream::connect_timeout(&addr, timeout);
    connect_inner(broadcast, addr, &connect)
}

/// Broadcast used by [`connect_to`] when none was received.
pub(crate) fn synthetic_broadcast() -> protocol::DacBroadcast {
    protocol::DacBroadcast {
        mac_address: [0; 6],
        hw_revision: 0,
        sw_revision: 0,
        buffer_capacity: SYNTHETIC_BUFFER_CAPACITY,
        max_point_rate: SYNTHETIC_MAX_POINT_RATE,
        dac_status: protocol::DacStatus {
            protocol: 0,
            light_engine_state: protocol::DacStatus::LIGHT_ENGINE_READY,
            playback_state: protocol::DacStatus::PLAYBACK_IDLE,
            source: protocol::DacStatus::SOURCE_NETWORK_STREAMING,
            light_engine_flags: 0,
            playback_flags: 0,
            source_flags: 0,
            buffer_fullness: 0,
            point_rate: 0,
            point_count: 0,
        },
    }
}

/// Capacity assumed when connecting without a broadcast.
pub(crate) const SYNTHETIC_BUFFER_CAPACITY: u16 = 1799;
/// Max point rate assumed when connecting without a broadcast.
pub(crate) const SYNTHETIC_MAX_POINT_RATE: u32 = 100_000;

/// Establishes a TCP stream connection with a timeout.
pub fn connect_timeout(
    broadcast: &protocol::DacBroadcast,
    dac_ip: net::IpAddr,
    timeout: time::Duration,
) -> Result<Stream, CommunicationError> {
    let connect = |addr| net::TcpStream::connect_timeout(&addr, timeout);
    let addr = net::SocketAddr::new(dac_ip, protocol::COMMUNICATION_PORT);
    connect_inner(broadcast, addr, &connect)
}

fn connect_inner(
    broadcast: &protocol::DacBroadcast,
    dac_addr: net::SocketAddr,
    connect: &dyn Fn(net::SocketAddr) -> io::Result<net::TcpStream>,
) -> Result<Stream, CommunicationError> {
    let mut dac = Addressed::from_broadcast(broadcast)?;

    let opened_at = time::Instant::now();
    let tcp_stream = connect(dac_addr)?;

    tcp_stream.set_nodelay(true)?;

    // Read timeout prevents blocking forever on a dead DAC. On a timeout the
    // in-progress read is retried (preserving bytes already consumed) up to
    // `READ_BUDGET` before failing — see `read_exact_with_budget`.
    tcp_stream.set_read_timeout(Some(READ_TIMEOUT))?;
    // Write timeout prevents a hung DAC from wedging the session thread forever
    // inside `write_all`.
    tcp_stream.set_write_timeout(Some(WRITE_TIMEOUT))?;

    let tcp_writer = tcp_stream.try_clone()?;
    let mut tcp_reader = BufReader::new(tcp_stream);

    let mut bytes = vec![];

    recv_response_buffered(
        &mut bytes,
        &mut tcp_reader,
        &mut dac,
        protocol::command::Ping::START_BYTE,
    )?;
    let hello_received_at = time::Instant::now();

    Ok(Stream {
        dac,
        tcp_reader,
        tcp_writer,
        command_buffer: vec![],
        point_buffer: vec![],
        bytes,
        status_sent_at: opened_at,
        status_received_at: hello_received_at,
    })
}

fn send_command<C>(
    bytes: &mut Vec<u8>,
    tcp_stream: &mut net::TcpStream,
    command: C,
) -> io::Result<()>
where
    C: Command + WriteToBytes,
{
    bytes.clear();
    bytes.write_bytes(command)?;
    tcp_stream.write_all(bytes)?;
    Ok(())
}

fn recv_response_buffered(
    bytes: &mut Vec<u8>,
    tcp_reader: &mut BufReader<net::TcpStream>,
    dac: &mut Addressed,
    expected_command: u8,
) -> Result<(), CommunicationError> {
    const MAX_RETRIES: usize = 5;

    for _ in 0..=MAX_RETRIES {
        bytes.resize(protocol::DacResponse::SIZE_BYTES, 0);
        read_exact_with_budget(tcp_reader, bytes, READ_BUDGET)?;
        let response = (&bytes[..]).read_bytes::<protocol::DacResponse>()?;

        // Always update status from every response, even mismatched ones.
        dac.update_status(&response.dac_status)?;

        if response.command == expected_command {
            response.check_errors(expected_command)?;
            return Ok(());
        }

        // Command mismatch — check if it's a stale/unsolicited frame we can skip.
        // ACK mismatch: stale pipeline response from a previous command batch.
        // NAK_INVALID ('I') mismatch: unsolicited status frame from the DAC.
        if response.response == protocol::DacResponse::ACK
            || response.response == protocol::DacResponse::NAK_INVALID
        {
            log::debug!(
                "ignoring unsolicited response (got command 0x{:02X}, expected 0x{:02X}, response 0x{:02X})",
                response.command,
                expected_command,
                response.response,
            );
            continue;
        }

        // Non-ACK, non-status mismatch — real error.
        return Err(CommunicationError::Response(ResponseError {
            response,
            kind: ResponseErrorKind::UnexpectedCommand(response.command),
        }));
    }

    Err(CommunicationError::Io(io::Error::other(
        "too many unsolicited responses",
    )))
}

/// Fill `buf` completely, retrying reads that time out (or are interrupted)
/// while tracking bytes already consumed, so a per-read timeout never desyncs
/// the stream by dropping a partial response. Fails once `budget` elapses.
fn read_exact_with_budget<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    budget: time::Duration,
) -> io::Result<()> {
    let start = time::Instant::now();
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-response",
                ));
            }
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(ref e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                // Partial data (if any) is retained in `buf[..filled]`; keep
                // waiting for the rest until the overall budget is exhausted.
                if start.elapsed() >= budget {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out reading response",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
