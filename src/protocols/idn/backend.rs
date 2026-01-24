//! IDN DAC streaming backend implementation.

use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::idn::dac::{stream, ServerInfo, ServiceInfo};
use crate::protocols::idn::error::{CommunicationError, ResponseError};
use crate::protocols::idn::protocol::PointXyrgbi;
use crate::types::{DacCapabilities, DacType, LaserPoint};
use std::time::Duration;

/// IDN DAC backend (ILDA Digital Network).
pub struct IdnBackend {
    server: ServerInfo,
    service: ServiceInfo,
    stream: Option<stream::Stream>,
    caps: DacCapabilities,
    /// Reusable scratch buffer for point conversion (eliminates per-chunk allocation).
    scratch: Vec<PointXyrgbi>,
}

impl IdnBackend {
    pub fn new(server: ServerInfo, service: ServiceInfo) -> Self {
        Self {
            server,
            service,
            stream: None,
            caps: super::default_capabilities(),
            scratch: Vec::new(),
        }
    }
}

impl StreamBackend for IdnBackend {
    fn dac_type(&self) -> DacType {
        DacType::Idn
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        let stream =
            stream::connect(&self.server, self.service.service_id).map_err(Error::backend)?;

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

        if stream.needs_keepalive() {
            stream
                .ping(Duration::from_millis(200))
                .map_err(|e| match e {
                    CommunicationError::Response(ResponseError::Timeout) => {
                        Error::disconnected("Connection lost: ping timeout")
                    }
                    e => Error::backend(e),
                })?;
        }

        stream.set_scan_speed(pps);

        // Reuse scratch buffer to avoid per-chunk allocation
        self.scratch.clear();
        self.scratch.extend(points.iter().map(PointXyrgbi::from));

        stream.write_frame(&self.scratch).map_err(Error::backend)?;

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
