//! DAC backend trait and implementations for the streaming API.
//!
//! This module provides the [`StreamBackend`] trait that all DAC backends must
//! implement, as well as implementations for all supported DAC types.

use crate::types::{caps_for_dac_type, Caps, DacType, LaserPoint};

// Re-export error types for backwards compatibility
pub use crate::error::{Error, Result};

// =============================================================================
// StreamBackend Trait
// =============================================================================

/// Write result from a backend chunk submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The chunk was accepted and written.
    Written,
    /// The device cannot accept more data right now.
    WouldBlock,
}

/// Backend trait for streaming DAC output.
///
/// All backends must implement this trait to support the streaming API.
/// The key contract is uniform backpressure: `try_write_chunk` must return
/// `WriteOutcome::WouldBlock` when the device cannot accept more data,
/// enabling the stream scheduler to pace output correctly.
pub trait StreamBackend: Send + 'static {
    /// Returns the DAC type for this backend.
    fn dac_type(&self) -> DacType;

    /// Returns the device capabilities.
    fn caps(&self) -> &Caps;

    /// Connect to the device.
    fn connect(&mut self) -> Result<()>;

    /// Disconnect from the device.
    fn disconnect(&mut self) -> Result<()>;

    /// Returns whether the device is connected.
    fn is_connected(&self) -> bool;

    /// Attempt to write a chunk of points at the given PPS.
    ///
    /// # Contract
    ///
    /// This is the core backpressure mechanism. Implementations must:
    ///
    /// 1. Return `WriteOutcome::WouldBlock` when the device cannot accept more data
    ///    (buffer full, not ready, etc.).
    /// 2. Return `WriteOutcome::Written` when the chunk was accepted.
    /// 3. Return `Err(...)` only for actual errors (disconnection, protocol errors).
    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome>;

    /// Stop output (if supported by the device).
    fn stop(&mut self) -> Result<()>;

    /// Open/close the shutter (if supported by the device).
    fn set_shutter(&mut self, open: bool) -> Result<()>;

    /// Best-effort estimate of points currently queued in the device.
    ///
    /// Not all devices can report this. Return `None` if unavailable.
    fn queued_points(&self) -> Option<u64> {
        None
    }
}

// =============================================================================
// Helios Backend
// =============================================================================

#[cfg(feature = "helios")]
mod helios_backend {
    use super::*;
    use crate::protocols::helios::{
        DeviceStatus, Frame, HeliosDac, HeliosDacController, Point as HeliosPoint,
    };

    /// Helios DAC backend (USB).
    pub struct HeliosBackend {
        dac: Option<HeliosDac>,
        device_index: usize,
        caps: Caps,
    }

    impl HeliosBackend {
        /// Create a new Helios backend for the given device index.
        pub fn new(device_index: usize) -> Self {
            Self {
                dac: None,
                device_index,
                caps: caps_for_dac_type(&DacType::Helios),
            }
        }

        /// Create a backend from an already-discovered DAC.
        pub fn from_dac(dac: HeliosDac) -> Self {
            Self {
                dac: Some(dac),
                device_index: 0,
                caps: caps_for_dac_type(&DacType::Helios),
            }
        }

        /// Discover all Helios DACs on the system.
        pub fn discover() -> Result<Vec<HeliosDac>> {
            let controller =
                HeliosDacController::new().map_err(|e| Error::backend(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to create controller: {}", e),
                )))?;
            controller.list_devices().map_err(|e| Error::backend(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to list devices: {}", e),
            )))
        }
    }

    impl StreamBackend for HeliosBackend {
        fn dac_type(&self) -> DacType {
            DacType::Helios
        }

        fn caps(&self) -> &Caps {
            &self.caps
        }

        fn connect(&mut self) -> Result<()> {
            if let Some(dac) = self.dac.take() {
                // Already have a DAC, try to open it if idle
                self.dac = Some(dac.open().map_err(|e| {
                    Error::backend(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Failed to open device: {}", e),
                    ))
                })?);
                return Ok(());
            }

            // Discover and open the device at the specified index
            let controller =
                HeliosDacController::new().map_err(|e| Error::backend(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to create controller: {}", e),
                )))?;
            let mut dacs = controller.list_devices().map_err(|e| Error::backend(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to list devices: {}", e),
            )))?;

            if self.device_index >= dacs.len() {
                return Err(Error::disconnected(format!(
                    "Device index {} out of range (found {} devices)",
                    self.device_index,
                    dacs.len()
                )));
            }

            let dac = dacs.remove(self.device_index);
            let dac = dac.open().map_err(|e| {
                Error::backend(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to open device: {}", e),
                ))
            })?;
            self.dac = Some(dac);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            self.dac = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            matches!(self.dac, Some(HeliosDac::Open { .. }))
        }

        fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
            let dac = self
                .dac
                .as_mut()
                .ok_or_else(|| Error::disconnected("Not connected"))?;

            // Check device status
            match dac.status() {
                Ok(DeviceStatus::Ready) => {}
                Ok(DeviceStatus::NotReady) => return Ok(WriteOutcome::WouldBlock),
                Err(e) => {
                    return Err(Error::backend(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Failed to get status: {}", e),
                    )))
                }
            }

            // Convert LaserPoints to Helios Points
            let helios_points: Vec<HeliosPoint> = points.iter().map(|p| p.into()).collect();
            let helios_frame = Frame::new(pps, helios_points);

            dac.write_frame(helios_frame).map_err(|e| {
                Error::backend(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to write frame: {}", e),
                ))
            })?;

            Ok(WriteOutcome::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(dac) = &self.dac {
                dac.stop().map_err(|e| {
                    Error::backend(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Failed to stop: {}", e),
                    ))
                })?;
            }
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            // Helios doesn't have explicit shutter control
            Ok(())
        }
    }
}

#[cfg(feature = "helios")]
pub use helios_backend::HeliosBackend;

// =============================================================================
// Ether Dream Backend
// =============================================================================

#[cfg(feature = "ether-dream")]
mod ether_dream_backend {
    use super::*;
    use crate::protocols::ether_dream::dac::{stream, LightEngine, Playback, PlaybackFlags};
    use crate::protocols::ether_dream::protocol::{DacBroadcast, DacPoint};
    use std::net::IpAddr;
    use std::time::Duration;

    /// Ether Dream DAC backend (network).
    pub struct EtherDreamBackend {
        broadcast: DacBroadcast,
        ip_addr: IpAddr,
        stream: Option<stream::Stream>,
        caps: Caps,
    }

    impl EtherDreamBackend {
        pub fn new(broadcast: DacBroadcast, ip_addr: IpAddr) -> Self {
            Self {
                broadcast,
                ip_addr,
                stream: None,
                caps: caps_for_dac_type(&DacType::EtherDream),
            }
        }
    }

    impl StreamBackend for EtherDreamBackend {
        fn dac_type(&self) -> DacType {
            DacType::EtherDream
        }

        fn caps(&self) -> &Caps {
            &self.caps
        }

        fn connect(&mut self) -> Result<()> {
            let stream =
                stream::connect_timeout(&self.broadcast, self.ip_addr, Duration::from_secs(5))
                    .map_err(|e| Error::backend(e))?;

            self.stream = Some(stream);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                let _ = stream.queue_commands().stop().submit();
            }
            self.stream = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.stream.is_some()
        }

        fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| Error::disconnected("Not connected"))?;

            let dac_points: Vec<DacPoint> = points.iter().map(|p| p.into()).collect();
            if dac_points.is_empty() {
                return Ok(WriteOutcome::WouldBlock);
            }

            // Check light engine state
            let light_engine = stream.dac().status.light_engine;

            match light_engine {
                LightEngine::EmergencyStop => {
                    stream
                        .queue_commands()
                        .clear_emergency_stop()
                        .submit()
                        .map_err(|e| Error::backend(e))?;

                    stream
                        .queue_commands()
                        .ping()
                        .submit()
                        .map_err(|e| Error::backend(e))?;

                    if stream.dac().status.light_engine == LightEngine::EmergencyStop {
                        return Err(Error::disconnected(
                            "DAC stuck in emergency stop - check hardware interlock",
                        ));
                    }
                }
                LightEngine::Warmup | LightEngine::Cooldown => {
                    return Ok(WriteOutcome::WouldBlock);
                }
                LightEngine::Ready => {}
            }

            // Check buffer space
            let buffer_capacity = stream.dac().buffer_capacity;
            let buffer_fullness = stream.dac().status.buffer_fullness;
            let available = buffer_capacity as usize - buffer_fullness as usize - 1;

            if available == 0 {
                return Ok(WriteOutcome::WouldBlock);
            }

            let point_rate = if pps > 0 {
                pps
            } else {
                stream.dac().max_point_rate / 16
            };

            const MIN_POINTS_BEFORE_BEGIN: u16 = 500;
            let target_buffer_points =
                (point_rate / 20).max(MIN_POINTS_BEFORE_BEGIN as u32) as usize;
            let target_len = target_buffer_points
                .min(available)
                .max(dac_points.len().min(available));

            let mut points_to_send = dac_points;
            if points_to_send.len() > available {
                points_to_send.truncate(available);
            } else if points_to_send.len() < target_len {
                let seed = points_to_send.clone();
                points_to_send.extend(seed.iter().cycle().take(target_len - points_to_send.len()));
            }

            let playback_flags = stream.dac().status.playback_flags;
            let playback = stream.dac().status.playback;
            let current_point_rate = stream.dac().status.point_rate;

            let mut force_begin = false;
            if playback_flags.contains(PlaybackFlags::UNDERFLOWED) {
                stream
                    .queue_commands()
                    .prepare_stream()
                    .submit()
                    .map_err(|e| Error::backend(e))?;
                force_begin = true;
            }

            let result = if force_begin {
                stream
                    .queue_commands()
                    .data(points_to_send.clone())
                    .submit()
                    .map_err(|e| Error::backend(e))?;

                let buffer_fullness = stream.dac().status.buffer_fullness;

                if buffer_fullness >= MIN_POINTS_BEFORE_BEGIN {
                    stream
                        .queue_commands()
                        .begin(0, point_rate)
                        .submit()
                        .map_err(|e| Error::backend(e))
                } else {
                    Ok(())
                }
            } else {
                match playback {
                    Playback::Idle | Playback::Prepared => {
                        if playback == Playback::Idle {
                            stream
                                .queue_commands()
                                .prepare_stream()
                                .submit()
                                .map_err(|e| Error::backend(e))?;
                        }

                        stream
                            .queue_commands()
                            .data(points_to_send.clone())
                            .submit()
                            .map_err(|e| Error::backend(e))?;

                        let buffer_fullness = stream.dac().status.buffer_fullness;
                        if buffer_fullness >= MIN_POINTS_BEFORE_BEGIN {
                            stream
                                .queue_commands()
                                .begin(0, point_rate)
                                .submit()
                                .map_err(|e| Error::backend(e))
                        } else {
                            Ok(())
                        }
                    }
                    Playback::Playing => {
                        let send_result = if current_point_rate != point_rate {
                            stream
                                .queue_commands()
                                .update(0, point_rate)
                                .data(points_to_send.clone())
                                .submit()
                        } else {
                            stream.queue_commands().data(points_to_send.clone()).submit()
                        };

                        if send_result.is_err() {
                            let current_playback = stream.dac().status.playback;

                            if current_playback == Playback::Idle {
                                stream
                                    .queue_commands()
                                    .prepare_stream()
                                    .submit()
                                    .map_err(|e| Error::backend(e))?;

                                stream
                                    .queue_commands()
                                    .data(points_to_send.clone())
                                    .submit()
                                    .map_err(|e| Error::backend(e))?;

                                let buffer_fullness = stream.dac().status.buffer_fullness;
                                if buffer_fullness >= MIN_POINTS_BEFORE_BEGIN {
                                    stream
                                        .queue_commands()
                                        .begin(0, point_rate)
                                        .submit()
                                        .map_err(|e| Error::backend(e))
                                } else {
                                    Ok(())
                                }
                            } else {
                                send_result.map_err(|e| Error::backend(e))
                            }
                        } else {
                            send_result.map_err(|e| Error::backend(e))
                        }
                    }
                }
            };

            result?;
            Ok(WriteOutcome::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                stream
                    .queue_commands()
                    .stop()
                    .submit()
                    .map_err(|e| Error::backend(e))?;
            }
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            Ok(())
        }

        fn queued_points(&self) -> Option<u64> {
            self.stream
                .as_ref()
                .map(|s| s.dac().status.buffer_fullness as u64)
        }
    }
}

#[cfg(feature = "ether-dream")]
pub use ether_dream_backend::EtherDreamBackend;

// =============================================================================
// IDN Backend
// =============================================================================

#[cfg(feature = "idn")]
mod idn_backend {
    use super::*;
    use crate::protocols::idn::dac::{stream, ServerInfo, ServiceInfo};
    use crate::protocols::idn::error::{CommunicationError, ResponseError};
    use crate::protocols::idn::protocol::PointXyrgbi;
    use std::time::Duration;

    /// IDN DAC backend (ILDA Digital Network).
    pub struct IdnBackend {
        server: ServerInfo,
        service: ServiceInfo,
        stream: Option<stream::Stream>,
        caps: Caps,
    }

    impl IdnBackend {
        pub fn new(server: ServerInfo, service: ServiceInfo) -> Self {
            Self {
                server,
                service,
                stream: None,
                caps: caps_for_dac_type(&DacType::Idn),
            }
        }
    }

    impl StreamBackend for IdnBackend {
        fn dac_type(&self) -> DacType {
            DacType::Idn
        }

        fn caps(&self) -> &Caps {
            &self.caps
        }

        fn connect(&mut self) -> Result<()> {
            let stream = stream::connect(&self.server, self.service.service_id)
                .map_err(|e| Error::backend(e))?;

            self.stream = Some(stream);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                let _ = stream.close();
            }
            self.stream = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.stream.is_some()
        }

        fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| Error::disconnected("Not connected"))?;

            // Check if we need to send keepalive
            if stream.needs_keepalive() {
                match stream.ping(Duration::from_millis(200)) {
                    Ok(_) => {}
                    Err(CommunicationError::Response(ResponseError::Timeout)) => {
                        return Err(Error::disconnected("Connection lost: ping timeout"));
                    }
                    Err(e) => {
                        return Err(Error::backend(e));
                    }
                }
            }

            stream.set_scan_speed(pps);
            let idn_points: Vec<PointXyrgbi> = points.iter().map(|p| p.into()).collect();

            stream.write_frame(&idn_points).map_err(|e| Error::backend(e))?;

            Ok(WriteOutcome::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                let blank_frame = vec![PointXyrgbi::new(0, 0, 0, 0, 0, 0); 10];
                let _ = stream.write_frame(&blank_frame);
            }
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(feature = "idn")]
pub use idn_backend::IdnBackend;

// =============================================================================
// LaserCube WiFi Backend
// =============================================================================

#[cfg(feature = "lasercube-wifi")]
mod lasercube_wifi_backend {
    use super::*;
    use crate::protocols::lasercube_wifi::dac::{stream, Addressed};
    use crate::protocols::lasercube_wifi::protocol::{DeviceInfo, Point as LasercubePoint};
    use std::net::SocketAddr;

    /// LaserCube WiFi DAC backend.
    pub struct LasercubeWifiBackend {
        addressed: Addressed,
        stream: Option<stream::Stream>,
        caps: Caps,
    }

    impl LasercubeWifiBackend {
        pub fn new(addressed: Addressed) -> Self {
            Self {
                addressed,
                stream: None,
                caps: caps_for_dac_type(&DacType::LasercubeWifi),
            }
        }

        pub fn from_discovery(info: &DeviceInfo, source_addr: SocketAddr) -> Self {
            Self::new(Addressed::from_discovery(info, source_addr))
        }
    }

    impl StreamBackend for LasercubeWifiBackend {
        fn dac_type(&self) -> DacType {
            DacType::LasercubeWifi
        }

        fn caps(&self) -> &Caps {
            &self.caps
        }

        fn connect(&mut self) -> Result<()> {
            let stream =
                stream::connect(&self.addressed).map_err(|e| Error::backend(e))?;

            self.stream = Some(stream);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                let _ = stream.stop();
            }
            self.stream = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.stream.is_some()
        }

        fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| Error::disconnected("Not connected"))?;

            let lc_points: Vec<LasercubePoint> = points.iter().map(|p| p.into()).collect();

            stream
                .write_frame(&lc_points, pps)
                .map_err(|e| Error::backend(e))?;

            Ok(WriteOutcome::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                stream.stop().map_err(|e| Error::backend(e))?;
            }
            Ok(())
        }

        fn set_shutter(&mut self, open: bool) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                stream.set_output(open).map_err(|e| Error::backend(e))?;
            }
            Ok(())
        }

        fn queued_points(&self) -> Option<u64> {
            // LaserCube WiFi reports free buffer space, but we don't know the max buffer size
            // to calculate used points. Return None for now.
            None
        }
    }
}

#[cfg(feature = "lasercube-wifi")]
pub use lasercube_wifi_backend::LasercubeWifiBackend;

// =============================================================================
// LaserCube USB Backend
// =============================================================================

#[cfg(feature = "lasercube-usb")]
mod lasercube_usb_backend {
    use super::*;
    use crate::protocols::lasercube_usb::dac::Stream;
    use crate::protocols::lasercube_usb::protocol::Sample as LasercubeUsbSample;
    use crate::protocols::lasercube_usb::{discover_dacs, rusb};

    /// LaserCube USB DAC backend (LaserDock).
    pub struct LasercubeUsbBackend {
        device: Option<rusb::Device<rusb::Context>>,
        stream: Option<Stream<rusb::Context>>,
        caps: Caps,
    }

    impl LasercubeUsbBackend {
        pub fn new(device: rusb::Device<rusb::Context>) -> Self {
            Self {
                device: Some(device),
                stream: None,
                caps: caps_for_dac_type(&DacType::LasercubeUsb),
            }
        }

        pub fn from_stream(stream: Stream<rusb::Context>) -> Self {
            Self {
                device: None,
                stream: Some(stream),
                caps: caps_for_dac_type(&DacType::LasercubeUsb),
            }
        }

        pub fn discover_devices() -> Result<Vec<rusb::Device<rusb::Context>>> {
            discover_dacs().map_err(|e| Error::backend(e))
        }
    }

    impl StreamBackend for LasercubeUsbBackend {
        fn dac_type(&self) -> DacType {
            DacType::LasercubeUsb
        }

        fn caps(&self) -> &Caps {
            &self.caps
        }

        fn connect(&mut self) -> Result<()> {
            if self.stream.is_some() {
                return Ok(());
            }

            let device = self
                .device
                .take()
                .ok_or_else(|| Error::disconnected("No device available"))?;

            let mut stream = Stream::open(device).map_err(|e| Error::backend(e))?;

            stream.enable_output().map_err(|e| Error::backend(e))?;

            self.stream = Some(stream);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                let _ = stream.stop();
            }
            self.stream = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.stream.is_some()
        }

        fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| Error::disconnected("Not connected"))?;

            let samples: Vec<LasercubeUsbSample> = points.iter().map(|p| p.into()).collect();

            stream
                .write_frame(&samples, pps)
                .map_err(|e| Error::backend(e))?;

            Ok(WriteOutcome::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                stream.stop().map_err(|e| Error::backend(e))?;
            }
            Ok(())
        }

        fn set_shutter(&mut self, open: bool) -> Result<()> {
            if let Some(stream) = &mut self.stream {
                if open {
                    stream.enable_output().map_err(|e| Error::backend(e))?;
                } else {
                    stream.disable_output().map_err(|e| Error::backend(e))?;
                }
            }
            Ok(())
        }

        fn queued_points(&self) -> Option<u64> {
            // LaserCube USB reports free ringbuffer space, but requires &mut self
            // and we don't know the max buffer size to calculate used points.
            // Return None for now.
            None
        }
    }
}

#[cfg(feature = "lasercube-usb")]
pub use lasercube_usb_backend::LasercubeUsbBackend;
