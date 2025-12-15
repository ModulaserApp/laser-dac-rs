//! DAC backend abstraction and point conversion.
//!
//! Provides a unified `DacBackend` trait for all DAC types and handles
//! point conversion from `LaserFrame` to device-specific formats.

use crate::error::{Error, Result};
use crate::types::{DacType, LaserFrame};

// =============================================================================
// DAC Backend Trait
// =============================================================================

/// Result of attempting to write a frame to a DAC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteResult {
    /// Frame was successfully written.
    Written,
    /// Device was busy, frame was dropped.
    DeviceBusy,
}

/// Unified interface for all DAC backends.
///
/// Each backend handles its own point conversion and device-specific protocol.
pub trait DacBackend: Send + 'static {
    /// Get the DAC type.
    fn dac_type(&self) -> DacType;

    /// Connect to the DAC.
    fn connect(&mut self) -> Result<()>;

    /// Disconnect from the DAC.
    fn disconnect(&mut self) -> Result<()>;

    /// Check if connected to the DAC.
    fn is_connected(&self) -> bool;

    /// Write a frame to the DAC.
    ///
    /// Returns `Ok(WriteResult::Written)` on success, `Ok(WriteResult::DeviceBusy)`
    /// if the device couldn't accept the frame, or `Err` on connection failure.
    fn write_frame(&mut self, frame: &LaserFrame) -> Result<WriteResult>;

    /// Stop laser output.
    fn stop(&mut self) -> Result<()>;

    /// Set shutter state (open = laser enabled, closed = laser disabled).
    fn set_shutter(&mut self, open: bool) -> Result<()>;
}

// =============================================================================
// Conditional Backend Implementations
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
    }

    impl HeliosBackend {
        /// Create a new Helios backend for the given device index.
        pub fn new(device_index: usize) -> Self {
            Self {
                dac: None,
                device_index,
            }
        }

        /// Create a backend from an already-discovered DAC.
        pub fn from_dac(dac: HeliosDac) -> Self {
            Self {
                dac: Some(dac),
                device_index: 0,
            }
        }

        /// Discover all Helios DACs on the system.
        pub fn discover() -> Result<Vec<HeliosDac>> {
            let controller = HeliosDacController::new()
                .map_err(|e| Error::context("Failed to create controller", e))?;
            controller
                .list_devices()
                .map_err(|e| Error::context("Failed to list devices", e))
        }
    }

    impl DacBackend for HeliosBackend {
        fn dac_type(&self) -> DacType {
            DacType::Helios
        }

        fn connect(&mut self) -> Result<()> {
            if self.dac.is_some() {
                // Already have a DAC, try to open it if idle
                if let Some(dac) = self.dac.take() {
                    let dac = dac
                        .open()
                        .map_err(|e| Error::context("Failed to open device", e))?;
                    self.dac = Some(dac);
                }
                return Ok(());
            }

            // Discover and open the device at the specified index
            let controller = HeliosDacController::new()
                .map_err(|e| Error::context("Failed to create controller", e))?;
            let mut dacs = controller
                .list_devices()
                .map_err(|e| Error::context("Failed to list devices", e))?;

            if self.device_index >= dacs.len() {
                return Err(Error::msg(format!(
                    "Device index {} out of range (found {} devices)",
                    self.device_index,
                    dacs.len()
                )));
            }

            let dac = dacs.remove(self.device_index);
            let dac = dac
                .open()
                .map_err(|e| Error::context("Failed to open device", e))?;
            self.dac = Some(dac);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            // HeliosDac doesn't have an explicit close; it closes when dropped
            self.dac = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            matches!(self.dac, Some(HeliosDac::Open { .. }))
        }

        fn write_frame(&mut self, frame: &LaserFrame) -> Result<WriteResult> {
            let dac = self
                .dac
                .as_mut()
                .ok_or_else(|| Error::msg("Not connected"))?;

            // Check device status
            match dac.status() {
                Ok(DeviceStatus::Ready) => {}
                Ok(DeviceStatus::NotReady) => return Ok(WriteResult::DeviceBusy),
                Err(e) => return Err(Error::context("Failed to get status", e)),
            }

            // Convert LaserFrame to Helios Frame
            let helios_points: Vec<HeliosPoint> = frame.points.iter().map(|p| p.into()).collect();

            let helios_frame = Frame::new(frame.pps, helios_points);

            dac.write_frame(helios_frame)
                .map_err(|e| Error::context("Failed to write frame", e))?;

            Ok(WriteResult::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(ref dac) = self.dac {
                dac.stop()
                    .map_err(|e| Error::context("Failed to stop", e))?;
            }
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            // The helios-dac crate doesn't expose a shutter control method
            // Shutter state is implicitly controlled by output state
            Ok(())
        }
    }
}

#[cfg(feature = "helios")]
pub use helios_backend::HeliosBackend;

#[cfg(feature = "ether-dream")]
mod ether_dream_backend {
    use super::*;
    use crate::protocols::ether_dream::dac::{stream, Playback};
    use crate::protocols::ether_dream::protocol::{DacBroadcast, DacPoint};
    use std::net::IpAddr;
    use std::time::Duration;

    /// Ether Dream DAC backend (network).
    pub struct EtherDreamBackend {
        broadcast: DacBroadcast,
        ip_addr: IpAddr,
        stream: Option<stream::Stream>,
    }

    impl EtherDreamBackend {
        pub fn new(broadcast: DacBroadcast, ip_addr: IpAddr) -> Self {
            Self {
                broadcast,
                ip_addr,
                stream: None,
            }
        }
    }

    impl DacBackend for EtherDreamBackend {
        fn dac_type(&self) -> DacType {
            DacType::EtherDream
        }

        fn connect(&mut self) -> Result<()> {
            let stream =
                stream::connect_timeout(&self.broadcast, self.ip_addr, Duration::from_secs(5))
                    .map_err(|e| Error::context("Failed to connect", e))?;

            self.stream = Some(stream);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                let _ = stream.queue_commands().stop().submit();
            }
            self.stream = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.stream.is_some()
        }

        fn write_frame(&mut self, frame: &LaserFrame) -> Result<WriteResult> {
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| Error::msg("Not connected"))?;

            let points: Vec<DacPoint> = frame.points.iter().map(|p| p.into()).collect();

            // Check buffer space
            let available = stream.dac().buffer_capacity as usize
                - stream.dac().status.buffer_fullness as usize
                - 1;

            let to_send = points.len().min(available);
            if to_send == 0 {
                return Ok(WriteResult::DeviceBusy);
            }

            let point_rate = if frame.pps > 0 {
                frame.pps
            } else {
                stream.dac().max_point_rate / 16
            };

            let status = &stream.dac().status;
            let result = match status.playback {
                Playback::Idle => {
                    stream
                        .queue_commands()
                        .prepare_stream()
                        .submit()
                        .map_err(|e| Error::context("Failed to prepare stream", e))?;

                    stream
                        .queue_commands()
                        .data(points.into_iter().take(to_send))
                        .begin(0, point_rate)
                        .submit()
                }
                Playback::Prepared => stream
                    .queue_commands()
                    .data(points.into_iter().take(to_send))
                    .begin(0, point_rate)
                    .submit(),
                Playback::Playing => {
                    let current_rate = status.point_rate;
                    if current_rate != frame.pps {
                        stream
                            .queue_commands()
                            .point_rate(frame.pps)
                            .data(points.into_iter().take(to_send))
                            .submit()
                    } else {
                        stream
                            .queue_commands()
                            .data(points.into_iter().take(to_send))
                            .submit()
                    }
                }
            };

            result.map_err(|e| Error::context("Failed to write frame", e))?;
            Ok(WriteResult::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                stream
                    .queue_commands()
                    .stop()
                    .submit()
                    .map_err(|e| Error::context("Failed to stop", e))?;
            }
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            // Ether Dream doesn't have explicit shutter control
            Ok(())
        }
    }
}

#[cfg(feature = "ether-dream")]
pub use ether_dream_backend::EtherDreamBackend;

#[cfg(feature = "idn")]
mod idn_backend {
    use super::*;
    use crate::protocols::idn::dac::{stream, ServerInfo, ServiceInfo};
    use crate::protocols::idn::protocol::PointXyrgbi;

    /// IDN DAC backend (ILDA Digital Network).
    pub struct IdnBackend {
        server: ServerInfo,
        service: ServiceInfo,
        stream: Option<stream::Stream>,
    }

    impl IdnBackend {
        pub fn new(server: ServerInfo, service: ServiceInfo) -> Self {
            Self {
                server,
                service,
                stream: None,
            }
        }
    }

    impl DacBackend for IdnBackend {
        fn dac_type(&self) -> DacType {
            DacType::Idn
        }

        fn connect(&mut self) -> Result<()> {
            let stream = stream::connect(&self.server, self.service.service_id)
                .map_err(|e| Error::context("Failed to connect", e))?;

            self.stream = Some(stream);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                let _ = stream.close();
            }
            self.stream = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.stream.is_some()
        }

        fn write_frame(&mut self, frame: &LaserFrame) -> Result<WriteResult> {
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| Error::msg("Not connected"))?;

            stream.set_scan_speed(frame.pps);
            let points: Vec<PointXyrgbi> = frame.points.iter().map(|p| p.into()).collect();

            stream
                .write_frame(&points)
                .map_err(|e| Error::context("Failed to write frame", e))?;

            Ok(WriteResult::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                let blank_point = PointXyrgbi::new(0, 0, 0, 0, 0, 0);
                let blank_frame = vec![blank_point; 10];
                let _ = stream.write_frame(&blank_frame);
            }
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            // IDN doesn't have explicit shutter control
            Ok(())
        }
    }
}

#[cfg(feature = "idn")]
pub use idn_backend::IdnBackend;

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
    }

    impl LasercubeWifiBackend {
        pub fn new(addressed: Addressed) -> Self {
            Self {
                addressed,
                stream: None,
            }
        }

        pub fn from_discovery(info: &DeviceInfo, source_addr: SocketAddr) -> Self {
            Self::new(Addressed::from_discovery(info, source_addr))
        }
    }

    impl DacBackend for LasercubeWifiBackend {
        fn dac_type(&self) -> DacType {
            DacType::LasercubeWifi
        }

        fn connect(&mut self) -> Result<()> {
            let stream = stream::connect(&self.addressed)
                .map_err(|e| Error::context("Failed to connect", e))?;

            self.stream = Some(stream);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                let _ = stream.stop();
            }
            self.stream = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.stream.is_some()
        }

        fn write_frame(&mut self, frame: &LaserFrame) -> Result<WriteResult> {
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| Error::msg("Not connected"))?;

            let lc_points: Vec<LasercubePoint> = frame.points.iter().map(|p| p.into()).collect();

            stream
                .write_frame(&lc_points, frame.pps)
                .map_err(|e| Error::context("Failed to write frame", e))?;

            Ok(WriteResult::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                stream
                    .stop()
                    .map_err(|e| Error::context("Failed to stop", e))?;
            }
            Ok(())
        }

        fn set_shutter(&mut self, open: bool) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                stream
                    .set_output(open)
                    .map_err(|e| Error::context("Failed to set shutter", e))?;
            }
            Ok(())
        }
    }
}

#[cfg(feature = "lasercube-wifi")]
pub use lasercube_wifi_backend::LasercubeWifiBackend;

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
    }

    impl LasercubeUsbBackend {
        pub fn new(device: rusb::Device<rusb::Context>) -> Self {
            Self {
                device: Some(device),
                stream: None,
            }
        }

        pub fn from_stream(stream: Stream<rusb::Context>) -> Self {
            Self {
                device: None,
                stream: Some(stream),
            }
        }

        pub fn discover_devices() -> Result<Vec<rusb::Device<rusb::Context>>> {
            discover_dacs().map_err(|e| Error::context("Failed to discover devices", e))
        }
    }

    impl DacBackend for LasercubeUsbBackend {
        fn dac_type(&self) -> DacType {
            DacType::LasercubeUsb
        }

        fn connect(&mut self) -> Result<()> {
            if self.stream.is_some() {
                return Ok(());
            }

            let device = self
                .device
                .take()
                .ok_or_else(|| Error::msg("No device available"))?;

            let mut stream =
                Stream::open(device).map_err(|e| Error::context("Failed to open device", e))?;

            stream
                .enable_output()
                .map_err(|e| Error::context("Failed to enable output", e))?;

            self.stream = Some(stream);
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                let _ = stream.stop();
            }
            self.stream = None;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.stream.is_some()
        }

        fn write_frame(&mut self, frame: &LaserFrame) -> Result<WriteResult> {
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| Error::msg("Not connected"))?;

            let samples: Vec<LasercubeUsbSample> = frame.points.iter().map(|p| p.into()).collect();

            stream
                .write_frame(&samples, frame.pps)
                .map_err(|e| Error::context("Failed to write frame", e))?;

            Ok(WriteResult::Written)
        }

        fn stop(&mut self) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                stream
                    .stop()
                    .map_err(|e| Error::context("Failed to stop", e))?;
            }
            Ok(())
        }

        fn set_shutter(&mut self, open: bool) -> Result<()> {
            if let Some(ref mut stream) = self.stream {
                if open {
                    stream
                        .enable_output()
                        .map_err(|e| Error::context("Failed to enable output", e))?;
                } else {
                    stream
                        .disable_output()
                        .map_err(|e| Error::context("Failed to disable output", e))?;
                }
            }
            Ok(())
        }
    }
}

#[cfg(feature = "lasercube-usb")]
pub use lasercube_usb_backend::LasercubeUsbBackend;
