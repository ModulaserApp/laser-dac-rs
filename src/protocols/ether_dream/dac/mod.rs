//! A simple abstraction around a single Ether Dream DAC.

pub mod stream;

pub use self::stream::Stream;
use crate::protocols::ether_dream::protocol::{
    self, command, Command as CommandTrait, ReadFromBytes, WriteToBytes,
};
use bitflags::bitflags;
use std::error::Error;
use std::{fmt, io, ops};

/// A DAC along with its broadcasted MAC address.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Addressed {
    pub mac_address: MacAddress,
    pub dac: Dac,
}

/// A simple abstraction around a single Ether Dream DAC.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Dac {
    pub hw_revision: u16,
    pub sw_revision: u16,
    pub buffer_capacity: u16,
    pub max_point_rate: u32,
    pub status: Status,
}

/// The fixed-size array used to represent the MAC address of a DAC.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct MacAddress(pub [u8; 6]);

/// DAC status information.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct Status {
    pub protocol: u8,
    pub light_engine: LightEngine,
    pub playback: Playback,
    pub data_source: DataSource,
    pub light_engine_flags: LightEngineFlags,
    pub playback_flags: PlaybackFlags,
    pub buffer_fullness: u16,
    pub point_rate: u32,
    pub point_count: u32,
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum LightEngine {
    Ready,
    Warmup,
    Cooldown,
    EmergencyStop,
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum Playback {
    Idle,
    Prepared,
    Playing,
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum DataSource {
    NetworkStreaming,
    IldaPlayback(IldaPlaybackFlags),
    InternalAbstractGenerator(InternalAbstractGeneratorFlags),
}

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    pub struct LightEngineFlags: u16 {
        const EMERGENCY_STOP_PACKET_OR_INVALID_COMMAND = 0b00000001;
        const EMERGENCY_STOP_PROJECTOR_INPUT = 0b00000010;
        const EMERGENCY_STOP_PROJECTOR_INPUT_ACTIVE = 0b00000100;
        const EMERGENCY_STOP_OVER_TEMPERATURE = 0b00001000;
        const EMERGENCY_STOP_OVER_TEMPERATURE_ACTIVE = 0b00010000;
        const EMERGENCY_STOP_LOST_ETHERNET_LINK = 0b00100000;
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    pub struct PlaybackFlags: u16 {
        const SHUTTER_OPEN = 0b00000001;
        const UNDERFLOWED = 0b00000010;
        const EMERGENCY_STOP = 0b00000100;
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    pub struct IldaPlaybackFlags: u16 {
        const PLAYING = 0b0;
        const REPEAT = 0b1;
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    pub struct InternalAbstractGeneratorFlags: u16 {
        const PLAYING = 0;
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    pub struct PointControl: u16 {
        const CHANGE_RATE = 0b10000000_00000000;
    }
}

/// A runtime representation of any command.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Command<'a> {
    PrepareStream(command::PrepareStream),
    Begin(command::Begin),
    Update(command::Update),
    PointRate(command::PointRate),
    Data(command::Data<'a>),
    Stop(command::Stop),
    EmergencyStop(command::EmergencyStop),
    ClearEmergencyStop(command::ClearEmergencyStop),
    Ping(command::Ping),
}

#[derive(Debug)]
pub enum ProtocolError {
    UnknownLightEngineState,
    UnknownPlaybackState,
    UnknownDataSource,
}

impl Addressed {
    pub fn from_broadcast(dac_broadcast: &protocol::DacBroadcast) -> Result<Self, ProtocolError> {
        let protocol::DacBroadcast {
            mac_address,
            hw_revision,
            sw_revision,
            buffer_capacity,
            max_point_rate,
            dac_status,
        } = *dac_broadcast;
        let mac_address = MacAddress(mac_address);
        let status = Status::from_protocol(&dac_status)?;
        let dac = Dac {
            hw_revision,
            sw_revision,
            buffer_capacity,
            max_point_rate,
            status,
        };
        Ok(Addressed { mac_address, dac })
    }
}

impl Dac {
    pub fn update_status(&mut self, status: &protocol::DacStatus) -> Result<(), ProtocolError> {
        self.status.update(status)
    }
}

impl Status {
    pub fn from_protocol(status: &protocol::DacStatus) -> Result<Self, ProtocolError> {
        let protocol = status.protocol;
        let light_engine = LightEngine::from_protocol(status.light_engine_state)
            .ok_or(ProtocolError::UnknownLightEngineState)?;
        let playback = Playback::from_protocol(status.playback_state)
            .ok_or(ProtocolError::UnknownPlaybackState)?;
        let data_source = DataSource::from_protocol(status.source, status.source_flags)
            .ok_or(ProtocolError::UnknownDataSource)?;
        let light_engine_flags = LightEngineFlags::from_bits_truncate(status.light_engine_flags);
        let playback_flags = PlaybackFlags::from_bits_truncate(status.playback_flags);
        Ok(Status {
            protocol,
            light_engine,
            playback,
            data_source,
            light_engine_flags,
            playback_flags,
            buffer_fullness: status.buffer_fullness,
            point_rate: status.point_rate,
            point_count: status.point_count,
        })
    }

    pub fn update(&mut self, status: &protocol::DacStatus) -> Result<(), ProtocolError> {
        self.protocol = status.protocol;
        self.light_engine = LightEngine::from_protocol(status.light_engine_state)
            .ok_or(ProtocolError::UnknownLightEngineState)?;
        self.playback = Playback::from_protocol(status.playback_state)
            .ok_or(ProtocolError::UnknownPlaybackState)?;
        self.data_source = DataSource::from_protocol(status.source, status.source_flags)
            .ok_or(ProtocolError::UnknownDataSource)?;
        self.light_engine_flags = LightEngineFlags::from_bits_truncate(status.light_engine_flags);
        self.playback_flags = PlaybackFlags::from_bits_truncate(status.playback_flags);
        self.buffer_fullness = status.buffer_fullness;
        self.point_rate = status.point_rate;
        self.point_count = status.point_count;
        Ok(())
    }

    pub fn to_protocol(&self) -> protocol::DacStatus {
        let (source, source_flags) = self.data_source.to_protocol();
        protocol::DacStatus {
            protocol: self.protocol,
            light_engine_state: self.light_engine.to_protocol(),
            playback_state: self.playback.to_protocol(),
            source,
            light_engine_flags: self.light_engine_flags.bits(),
            playback_flags: self.playback_flags.bits(),
            source_flags,
            buffer_fullness: self.buffer_fullness,
            point_rate: self.point_rate,
            point_count: self.point_count,
        }
    }
}

impl LightEngine {
    pub fn from_protocol(state: u8) -> Option<Self> {
        match state {
            protocol::DacStatus::LIGHT_ENGINE_READY => Some(LightEngine::Ready),
            protocol::DacStatus::LIGHT_ENGINE_WARMUP => Some(LightEngine::Warmup),
            protocol::DacStatus::LIGHT_ENGINE_COOLDOWN => Some(LightEngine::Cooldown),
            protocol::DacStatus::LIGHT_ENGINE_EMERGENCY_STOP => Some(LightEngine::EmergencyStop),
            _ => None,
        }
    }

    pub fn to_protocol(&self) -> u8 {
        match *self {
            LightEngine::Ready => protocol::DacStatus::LIGHT_ENGINE_READY,
            LightEngine::Warmup => protocol::DacStatus::LIGHT_ENGINE_WARMUP,
            LightEngine::Cooldown => protocol::DacStatus::LIGHT_ENGINE_COOLDOWN,
            LightEngine::EmergencyStop => protocol::DacStatus::LIGHT_ENGINE_EMERGENCY_STOP,
        }
    }
}

impl Playback {
    pub fn from_protocol(state: u8) -> Option<Self> {
        match state {
            protocol::DacStatus::PLAYBACK_IDLE => Some(Playback::Idle),
            protocol::DacStatus::PLAYBACK_PREPARED => Some(Playback::Prepared),
            protocol::DacStatus::PLAYBACK_PLAYING => Some(Playback::Playing),
            _ => None,
        }
    }

    pub fn to_protocol(&self) -> u8 {
        match *self {
            Playback::Idle => protocol::DacStatus::PLAYBACK_IDLE,
            Playback::Prepared => protocol::DacStatus::PLAYBACK_PREPARED,
            Playback::Playing => protocol::DacStatus::PLAYBACK_PLAYING,
        }
    }
}

impl DataSource {
    pub fn from_protocol(source: u8, flags: u16) -> Option<Self> {
        match source {
            protocol::DacStatus::SOURCE_NETWORK_STREAMING => Some(DataSource::NetworkStreaming),
            protocol::DacStatus::SOURCE_ILDA_PLAYBACK_SD => Some(DataSource::IldaPlayback(
                IldaPlaybackFlags::from_bits_truncate(flags),
            )),
            protocol::DacStatus::SOURCE_INTERNAL_ABSTRACT_GENERATOR => {
                Some(DataSource::InternalAbstractGenerator(
                    InternalAbstractGeneratorFlags::from_bits_truncate(flags),
                ))
            }
            _ => None,
        }
    }

    pub fn to_protocol(&self) -> (u8, u16) {
        match *self {
            DataSource::NetworkStreaming => (protocol::DacStatus::SOURCE_NETWORK_STREAMING, 0),
            DataSource::IldaPlayback(ref flags) => {
                (protocol::DacStatus::SOURCE_ILDA_PLAYBACK_SD, flags.bits())
            }
            DataSource::InternalAbstractGenerator(ref flags) => (
                protocol::DacStatus::SOURCE_INTERNAL_ABSTRACT_GENERATOR,
                flags.bits(),
            ),
        }
    }
}

impl ops::Deref for Addressed {
    type Target = Dac;
    fn deref(&self) -> &Self::Target {
        &self.dac
    }
}

impl ops::DerefMut for Addressed {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.dac
    }
}

impl From<[u8; 6]> for MacAddress {
    fn from(bytes: [u8; 6]) -> Self {
        MacAddress(bytes)
    }
}

impl From<MacAddress> for [u8; 6] {
    fn from(mac: MacAddress) -> [u8; 6] {
        mac.0
    }
}

impl ops::Deref for MacAddress {
    type Target = [u8; 6];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let a = &self.0;
        write!(
            f,
            "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            a[0], a[1], a[2], a[3], a[4], a[5]
        )
    }
}

impl ReadFromBytes for Command<'static> {
    fn read_from_bytes<R: byteorder::ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let cmd = reader.read_u8()?;
        let kind = match cmd {
            command::PrepareStream::START_BYTE => command::PrepareStream.into(),
            command::Begin::START_BYTE => command::Begin::read_fields(reader)?.into(),
            command::Update::START_BYTE => command::Update::read_fields(reader)?.into(),
            command::PointRate::START_BYTE => command::PointRate::read_fields(reader)?.into(),
            command::Data::START_BYTE => command::Data::read_fields(reader)?.into(),
            command::Stop::START_BYTE => command::Stop.into(),
            command::EmergencyStop::START_BYTE => command::EmergencyStop.into(),
            command::EmergencyStopAlt::START_BYTE => command::EmergencyStop.into(),
            command::ClearEmergencyStop::START_BYTE => command::ClearEmergencyStop.into(),
            command::Ping::START_BYTE => command::Ping.into(),
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid command byte \"{}\"", unknown),
                ))
            }
        };
        Ok(kind)
    }
}

impl<'a> WriteToBytes for Command<'a> {
    fn write_to_bytes<W: byteorder::WriteBytesExt>(&self, writer: W) -> io::Result<()> {
        match *self {
            Command::PrepareStream(ref cmd) => cmd.write_to_bytes(writer),
            Command::Begin(ref cmd) => cmd.write_to_bytes(writer),
            Command::Update(ref cmd) => cmd.write_to_bytes(writer),
            Command::PointRate(ref cmd) => cmd.write_to_bytes(writer),
            Command::Data(ref cmd) => cmd.write_to_bytes(writer),
            Command::Stop(ref cmd) => cmd.write_to_bytes(writer),
            Command::EmergencyStop(ref cmd) => cmd.write_to_bytes(writer),
            Command::ClearEmergencyStop(ref cmd) => cmd.write_to_bytes(writer),
            Command::Ping(ref cmd) => cmd.write_to_bytes(writer),
        }
    }
}

impl<'a> From<command::PrepareStream> for Command<'a> {
    fn from(command: command::PrepareStream) -> Self {
        Command::PrepareStream(command)
    }
}
impl<'a> From<command::Begin> for Command<'a> {
    fn from(command: command::Begin) -> Self {
        Command::Begin(command)
    }
}
impl<'a> From<command::Update> for Command<'a> {
    fn from(command: command::Update) -> Self {
        Command::Update(command)
    }
}
impl<'a> From<command::PointRate> for Command<'a> {
    fn from(command: command::PointRate) -> Self {
        Command::PointRate(command)
    }
}
impl<'a> From<command::Data<'a>> for Command<'a> {
    fn from(command: command::Data<'a>) -> Self {
        Command::Data(command)
    }
}
impl<'a> From<command::Stop> for Command<'a> {
    fn from(command: command::Stop) -> Self {
        Command::Stop(command)
    }
}
impl<'a> From<command::EmergencyStop> for Command<'a> {
    fn from(command: command::EmergencyStop) -> Self {
        Command::EmergencyStop(command)
    }
}
impl<'a> From<command::EmergencyStopAlt> for Command<'a> {
    fn from(_: command::EmergencyStopAlt) -> Self {
        Command::EmergencyStop(command::EmergencyStop)
    }
}
impl<'a> From<command::ClearEmergencyStop> for Command<'a> {
    fn from(command: command::ClearEmergencyStop) -> Self {
        Command::ClearEmergencyStop(command)
    }
}
impl<'a> From<command::Ping> for Command<'a> {
    fn from(command: command::Ping) -> Self {
        Command::Ping(command)
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ProtocolError::UnknownLightEngineState => write!(f, "unknown light engine state"),
            ProtocolError::UnknownPlaybackState => write!(f, "unknown playback state"),
            ProtocolError::UnknownDataSource => write!(f, "unknown data source"),
        }
    }
}

impl Error for ProtocolError {}
