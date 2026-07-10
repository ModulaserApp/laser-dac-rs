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
        let caps = super::capabilities_for_status(addressed.profile, &addressed.status);
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

/// Hermetic loopback mock LaserCube DAC, shared by the backend and transport
/// handle tests. It binds the well-known LaserCube UDP ports on a unique
/// `127.0.0.x` loopback address (so parallel tests do not collide on the fixed
/// ports) and answers full-info requests and sample packets using the wire
/// encodings under test.
#[cfg(test)]
pub(crate) mod mock {
    use std::net::{IpAddr, SocketAddr, UdpSocket};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    use super::super::protocol::{
        CMD_GET_FULL_INFO, CMD_GET_RINGBUFFER_EMPTY, CMD_PORT, CMD_SAMPLE_DATA, DATA_PORT,
    };

    static NEXT_HOST: AtomicU32 = AtomicU32::new(0);

    /// Allocate a unique loopback IP so tests can share the fixed LaserCube
    /// ports without conflicting binds.
    pub(crate) fn unique_loopback_ip() -> IpAddr {
        let n = NEXT_HOST.fetch_add(1, Ordering::Relaxed);
        IpAddr::from([127, 0, 0, 101 + (n % 150) as u8])
    }

    /// A valid 64-byte full-info status advertising a 6000-sample buffer.
    pub(crate) fn full_info_status() -> [u8; 64] {
        let mut d = [0u8; 64];
        d[0] = CMD_GET_FULL_INFO;
        d[3] = 1; // firmware major
        d[4] = 24; // firmware minor
        d[10..14].copy_from_slice(&30_000u32.to_le_bytes()); // point_rate
        d[14..18].copy_from_slice(&30_000u32.to_le_bytes()); // point_rate_max
        d[19..21].copy_from_slice(&6000u16.to_le_bytes()); // buffer_free
        d[21..23].copy_from_slice(&6000u16.to_le_bytes()); // buffer_max
        d[25] = 0; // ethernet server
        d[37] = 10; // model number
        d[38..47].copy_from_slice(b"Ultra Mk2");
        d
    }

    /// Poll `predicate` until it holds or `timeout` elapses.
    pub(crate) fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    struct Shared {
        cmd_recv: Mutex<Vec<Vec<u8>>>,
        data_recv: Mutex<Vec<Vec<u8>>>,
    }

    pub(crate) struct MockDac {
        ip: IpAddr,
        shared: Arc<Shared>,
        stop: Arc<AtomicBool>,
        join: Option<JoinHandle<()>>,
    }

    impl MockDac {
        pub(crate) fn start() -> Self {
            let ip = unique_loopback_ip();
            let cmd_socket =
                UdpSocket::bind(SocketAddr::new(ip, CMD_PORT)).expect("bind mock cmd socket");
            let data_socket =
                UdpSocket::bind(SocketAddr::new(ip, DATA_PORT)).expect("bind mock data socket");
            cmd_socket
                .set_read_timeout(Some(Duration::from_millis(5)))
                .unwrap();
            data_socket
                .set_read_timeout(Some(Duration::from_millis(5)))
                .unwrap();

            let shared = Arc::new(Shared {
                cmd_recv: Mutex::new(Vec::new()),
                data_recv: Mutex::new(Vec::new()),
            });
            let stop = Arc::new(AtomicBool::new(false));

            let thread_shared = shared.clone();
            let thread_stop = stop.clone();
            let join = thread::Builder::new()
                .name("mock-lasercube-dac".to_string())
                .spawn(move || {
                    let mut buf = [0u8; 1500];
                    while !thread_stop.load(Ordering::Relaxed) {
                        if let Ok((len, src)) = cmd_socket.recv_from(&mut buf) {
                            let packet = buf[..len].to_vec();
                            if packet.first() == Some(&CMD_GET_FULL_INFO) {
                                let _ = cmd_socket.send_to(&full_info_status(), src);
                            }
                            thread_shared.cmd_recv.lock().unwrap().push(packet);
                        }
                        if let Ok((len, src)) = data_socket.recv_from(&mut buf) {
                            let packet = buf[..len].to_vec();
                            if packet.first() == Some(&CMD_SAMPLE_DATA) && packet.len() >= 3 {
                                // Echo the packet sequence (byte 2) and report
                                // 3000 free samples (0x0BB8).
                                let ack = [CMD_GET_RINGBUFFER_EMPTY, packet[2], 0xB8, 0x0B];
                                let _ = data_socket.send_to(&ack, src);
                            }
                            thread_shared.data_recv.lock().unwrap().push(packet);
                        }
                    }
                })
                .expect("spawn mock DAC thread");

            Self {
                ip,
                shared,
                stop,
                join: Some(join),
            }
        }

        pub(crate) fn ip(&self) -> IpAddr {
            self.ip
        }

        pub(crate) fn cmd_packets(&self) -> Vec<Vec<u8>> {
            self.shared.cmd_recv.lock().unwrap().clone()
        }

        pub(crate) fn data_packets(&self) -> Vec<Vec<u8>> {
            self.shared.data_recv.lock().unwrap().clone()
        }
    }

    impl Drop for MockDac {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, SocketAddr};
    use std::time::{Duration, Instant};

    use crate::device::OutputModel;
    use crate::point::LaserPoint;

    use super::super::profiles::{ConnectionProfile, ConnectionType};
    use super::super::protocol::CMD_PORT;
    use super::super::status::LaserCubeNetworkStatus;
    use super::mock::{self, MockDac};
    use super::*;

    fn addressed_for(ip: IpAddr) -> AddressedDevice {
        let mut status = LaserCubeNetworkStatus::minimal(ip);
        status.buffer_free = 6000;
        status.buffer_max = 6000;
        status.point_rate_max = 30_000;
        let profile = ConnectionProfile::for_connection(ConnectionType::EthernetServer, 6000);
        AddressedDevice {
            source_addr: SocketAddr::new(ip, CMD_PORT),
            status,
            profile,
        }
    }

    fn backend_for(dac: &MockDac) -> LaserCubeNetworkBackend {
        LaserCubeNetworkBackend::new(addressed_for(dac.ip()))
    }

    #[test]
    fn default_backend_reports_type_and_caps_and_is_disconnected() {
        let backend = LaserCubeNetworkBackend::default();
        assert_eq!(backend.dac_type(), DacType::LaserCubeNetwork);
        assert_eq!(backend.caps().output_model, OutputModel::NetworkFifo);
        assert!(!backend.is_connected());
        assert!(backend.diagnostics().is_none());
    }

    #[test]
    fn connect_disconnect_lifecycle_is_idempotent() {
        let dac = MockDac::start();
        let mut backend = backend_for(&dac);

        assert!(!backend.is_connected());
        assert!(backend.diagnostics().is_none());

        backend.connect().unwrap();
        assert!(backend.is_connected());
        assert!(backend.diagnostics().is_some());

        // Second connect is a no-op and keeps the connection.
        backend.connect().unwrap();
        assert!(backend.is_connected());

        backend.disconnect().unwrap();
        assert!(!backend.is_connected());
        assert!(backend.diagnostics().is_none());

        // Disconnect is idempotent.
        backend.disconnect().unwrap();
        assert!(!backend.is_connected());
    }

    #[test]
    fn try_write_points_requires_connection() {
        let mut backend = LaserCubeNetworkBackend::default();
        let err = backend
            .try_write_points(30_000, &[LaserPoint::default()])
            .unwrap_err();
        assert!(err.is_disconnected());
    }

    #[test]
    fn try_write_points_written_then_backpressure() {
        let dac = MockDac::start();
        let mut backend = backend_for(&dac);
        backend.connect().unwrap();

        let points = vec![LaserPoint::default(); 100];
        assert_eq!(
            backend.try_write_points(30_000, &points).unwrap(),
            WriteOutcome::Written
        );

        // A single write larger than the host-queue capacity (2 * buffer_max)
        // is rejected as backpressure, deterministically and regardless of the
        // worker's drain progress.
        let flood = vec![LaserPoint::default(); 20_000];
        assert_eq!(
            backend.try_write_points(30_000, &flood).unwrap(),
            WriteOutcome::WouldBlock
        );
    }

    #[test]
    fn estimator_is_wired_in_both_connection_states() {
        let dac = MockDac::start();
        let mut backend = backend_for(&dac);
        let now = Instant::now();

        // Disconnected: the fallback estimator answers.
        let _fallback = backend.estimator().estimated_fullness(now, 30_000);

        backend.connect().unwrap();
        // Connected: the shared transport-state estimator answers.
        let _connected = backend.estimator().estimated_fullness(now, 30_000);

        backend.disconnect().unwrap();
        // Back to the fallback estimator.
        let _fallback_again = backend.estimator().estimated_fullness(now, 30_000);
    }

    #[test]
    fn set_shutter_and_stop_are_noops_when_disconnected() {
        let mut backend = LaserCubeNetworkBackend::default();
        assert!(backend.set_shutter(true).is_ok());
        assert!(backend.set_shutter(false).is_ok());
        assert!(backend.stop().is_ok());
    }

    #[test]
    fn set_shutter_open_emits_output_enable_command() {
        let dac = MockDac::start();
        let mut backend = backend_for(&dac);
        backend.connect().unwrap();

        backend.set_shutter(true).unwrap();
        assert!(
            mock::wait_until(Duration::from_millis(1000), || dac
                .cmd_packets()
                .iter()
                .any(|p| p.as_slice() == [0x80, 0x01])),
            "expected set_output(true) command, got {:?}",
            dac.cmd_packets()
        );
    }

    #[test]
    fn stop_emits_additional_output_disable_command() {
        let dac = MockDac::start();
        let mut backend = backend_for(&dac);
        backend.connect().unwrap();

        // Let startup commands settle (they include one set_output(false)).
        mock::wait_until(Duration::from_millis(500), || !dac.cmd_packets().is_empty());
        let before = dac
            .cmd_packets()
            .iter()
            .filter(|p| p.as_slice() == [0x80, 0x00])
            .count();

        backend.stop().unwrap();
        assert!(
            mock::wait_until(Duration::from_millis(1000), || {
                dac.cmd_packets()
                    .iter()
                    .filter(|p| p.as_slice() == [0x80, 0x00])
                    .count()
                    > before
            }),
            "expected an additional set_output(false), got {:?}",
            dac.cmd_packets()
        );
    }

    #[test]
    fn write_points_reach_the_device_as_sample_packets() {
        let dac = MockDac::start();
        let mut backend = backend_for(&dac);
        backend.connect().unwrap();

        for _ in 0..5 {
            let points = vec![LaserPoint::default(); 200];
            let _ = backend.try_write_points(30_000, &points);
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            mock::wait_until(Duration::from_millis(1000), || dac
                .data_packets()
                .iter()
                .any(|p| p.first() == Some(&0xA9))),
            "expected sample-data packets at the device"
        );
    }
}
