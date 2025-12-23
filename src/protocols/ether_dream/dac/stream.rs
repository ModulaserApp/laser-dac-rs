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
        let end = self.stream.point_buffer.len();
        assert!(end - start < u16::MAX as usize, "too many points");
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

    pub fn submit(self) -> Result<(), CommunicationError> {
        let CommandQueue { stream } = self;

        let mut command_bytes = vec![];
        let mut command_buffer = mem::take(&mut stream.command_buffer);

        for command in command_buffer.drain(..) {
            match command {
                QueuedCommand::PrepareStream => {
                    stream.send_command(protocol::command::PrepareStream)?;
                    command_bytes.push(protocol::command::PrepareStream::START_BYTE);
                }
                QueuedCommand::Begin(begin) => {
                    stream.send_command(begin)?;
                    command_bytes.push(protocol::command::Begin::START_BYTE);
                }
                QueuedCommand::Update(update) => {
                    stream.send_command(update)?;
                    command_bytes.push(protocol::command::Update::START_BYTE);
                }
                QueuedCommand::PointRate(point_rate) => {
                    stream.send_command(point_rate)?;
                    command_bytes.push(protocol::command::PointRate::START_BYTE);
                }
                QueuedCommand::Data(range) => {
                    let points = Cow::Borrowed(&stream.point_buffer[range.clone()]);
                    let data = protocol::command::Data { points };
                    send_command(&mut stream.bytes, &mut stream.tcp_writer, data)?;
                    command_bytes.push(protocol::command::Data::START_BYTE);
                }
                QueuedCommand::Stop => {
                    stream.send_command(protocol::command::Stop)?;
                    command_bytes.push(protocol::command::Stop::START_BYTE);
                }
                QueuedCommand::EmergencyStop => {
                    stream.send_command(protocol::command::EmergencyStop)?;
                    command_bytes.push(protocol::command::EmergencyStop::START_BYTE);
                }
                QueuedCommand::ClearEmergencyStop => {
                    stream.send_command(protocol::command::ClearEmergencyStop)?;
                    command_bytes.push(protocol::command::ClearEmergencyStop::START_BYTE);
                }
                QueuedCommand::Ping => {
                    stream.send_command(protocol::command::Ping)?;
                    command_bytes.push(protocol::command::Ping::START_BYTE);
                }
            }
        }

        mem::swap(&mut stream.command_buffer, &mut command_buffer);

        for command_byte in command_bytes.drain(..) {
            stream.recv_response(command_byte)?;
        }

        Ok(())
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
        match nak {
            protocol::DacResponse::NAK_FULL => Some(Nak::Full),
            protocol::DacResponse::NAK_INVALID => Some(Nak::Invalid),
            protocol::DacResponse::NAK_STOP_CONDITION => Some(Nak::StopCondition),
            _ => None,
        }
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

/// Establishes a TCP stream connection with the DAC at the given address.
pub fn connect(
    broadcast: &protocol::DacBroadcast,
    dac_ip: net::IpAddr,
) -> Result<Stream, CommunicationError> {
    connect_inner(broadcast, dac_ip, &net::TcpStream::connect)
}

/// Establishes a TCP stream connection with a timeout.
pub fn connect_timeout(
    broadcast: &protocol::DacBroadcast,
    dac_ip: net::IpAddr,
    timeout: time::Duration,
) -> Result<Stream, CommunicationError> {
    let connect = |addr| net::TcpStream::connect_timeout(&addr, timeout);
    connect_inner(broadcast, dac_ip, &connect)
}

fn connect_inner(
    broadcast: &protocol::DacBroadcast,
    dac_ip: net::IpAddr,
    connect: &dyn Fn(net::SocketAddr) -> io::Result<net::TcpStream>,
) -> Result<Stream, CommunicationError> {
    let mut dac = Addressed::from_broadcast(broadcast)?;

    let dac_addr = net::SocketAddr::new(dac_ip, protocol::COMMUNICATION_PORT);
    let tcp_stream = connect(dac_addr)?;

    tcp_stream.set_nodelay(true)?;

    let tcp_writer = tcp_stream.try_clone()?;
    let mut tcp_reader = BufReader::new(tcp_stream);

    let mut bytes = vec![];

    recv_response_buffered(
        &mut bytes,
        &mut tcp_reader,
        &mut dac,
        protocol::command::Ping::START_BYTE,
    )?;

    Ok(Stream {
        dac,
        tcp_reader,
        tcp_writer,
        command_buffer: vec![],
        point_buffer: vec![],
        bytes,
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
    bytes.resize(protocol::DacResponse::SIZE_BYTES, 0);
    tcp_reader.read_exact(bytes)?;
    let response = (&bytes[..]).read_bytes::<protocol::DacResponse>()?;

    // Update status before returning errors so callers can react to DAC state
    // changes even when the response is a NAK.
    dac.update_status(&response.dac_status)?;
    response.check_errors(expected_command)?;
    Ok(())
}
