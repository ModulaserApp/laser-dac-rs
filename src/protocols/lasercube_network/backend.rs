//! LaserCube network backend integration.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::BufferEstimator;
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;

use super::diagnostics::LaserCubeNetworkDiagnostics;
use super::profiles::ConnectionProfile;
use super::protocol::{Point, DEFAULT_BUFFER_CAPACITY};
use super::transport::{AddressedDevice, SharedTransportState, TransportHandle};

pub struct LaserCubeNetworkBackend {
    addressed: AddressedDevice,
    transport: Option<TransportHandle>,
    caps: DacCapabilities,
    point_buffer: Vec<Point>,
    fallback_estimator: SharedTransportState,
}

impl LaserCubeNetworkBackend {
    pub(crate) fn new(addressed: AddressedDevice) -> Self {
        let caps = super::capabilities_for_profile(addressed.profile);
        Self {
            fallback_estimator: SharedTransportState::disconnected(addressed.profile),
            addressed,
            transport: None,
            caps,
            point_buffer: Vec::new(),
        }
    }

    pub fn diagnostics(&self) -> Option<LaserCubeNetworkDiagnostics> {
        self.transport.as_ref().map(|t| t.state().diagnostics())
    }
}

impl DacBackend for LaserCubeNetworkBackend {
    fn dac_type(&self) -> DacType {
        DacType::LaserCubeNetwork
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        if self.transport.is_some() {
            return Ok(());
        }
        let transport = TransportHandle::connect(self.addressed.clone()).map_err(Error::backend)?;
        self.transport = Some(transport);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(mut transport) = self.transport.take() {
            transport.shutdown();
        }
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.transport.as_ref().is_some_and(|t| t.is_usable())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(transport) = &self.transport {
            transport.stop_output().map_err(Error::backend)?;
        }
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        if let Some(transport) = &self.transport {
            transport.set_output(open).map_err(Error::backend)?;
        }
        Ok(())
    }
}

impl FifoBackend for LaserCubeNetworkBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let transport = self
            .transport
            .as_ref()
            .ok_or_else(|| Error::disconnected("LaserCube network backend is not connected"))?;
        if !transport.is_usable() {
            return Err(Error::disconnected(
                "LaserCube network communication timed out",
            ));
        }

        self.point_buffer.clear();
        self.point_buffer.extend(points.iter().map(Point::from));

        match transport.enqueue(pps, self.point_buffer.clone()) {
            Ok(()) => Ok(WriteOutcome::Written),
            Err(super::error::CommunicationError::QueueFull) => Ok(WriteOutcome::WouldBlock),
            Err(err) => Err(Error::backend(err)),
        }
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        match &self.transport {
            Some(t) => t.state(),
            None => &self.fallback_estimator,
        }
    }
}

impl Default for LaserCubeNetworkBackend {
    fn default() -> Self {
        let status = super::status::LaserCubeNetworkStatus::minimal(
            "0.0.0.0".parse().expect("valid default IP"),
        );
        let profile = ConnectionProfile::unknown_conservative(DEFAULT_BUFFER_CAPACITY as usize);
        Self::new(AddressedDevice {
            source_addr: "0.0.0.0:0".parse().expect("valid default socket address"),
            status,
            profile,
        })
    }
}
