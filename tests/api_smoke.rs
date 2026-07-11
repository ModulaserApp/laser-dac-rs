//! Smoke tests for the top-level device-discovery API in `src/lib.rs`.
//!
//! These exercise `open_device_with`, `list_devices_filtered`, and the
//! `Dac` metadata surface WITHOUT touching real USB/network hardware. A
//! custom [`Discoverer`] + mock FIFO backend are registered via a discovery
//! factory, so every path here is hermetic and feature-independent (only
//! `DacType::Custom` is used, which is always available). The file compiles
//! and passes under `--no-default-features`.

use std::any::Any;

use laser_dac::backend::{BackendKind, DacBackend, FifoBackend};
use laser_dac::buffer_estimate::{BufferEstimator, SoftwareDecayEstimator};
use laser_dac::device::OutputModel;
use laser_dac::{
    downcast_connect_data, list_devices_filtered, open_device_with, DacCapabilities, DacDiscovery,
    DacType, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer, EnabledDacTypes, LaserPoint,
    Result, WriteOutcome,
};

// =============================================================================
// Mock backend + discoverer (hardware-free)
// =============================================================================

/// Distinctive caps so tests can prove `Dac::caps()` comes from the backend
/// (via `open_discovered`), not from the discoverer's derived defaults.
const MOCK_CAPS: DacCapabilities = DacCapabilities {
    pps_min: 42,
    pps_max: 54_321,
    max_points_per_chunk: 777,
    output_model: OutputModel::NetworkFifo,
};

fn mock_dac_type() -> DacType {
    DacType::Custom("SmokeDac".into())
}

struct MockConnectData {
    _serial: u32,
}

struct MockBackend {
    connected: bool,
    estimator: SoftwareDecayEstimator,
}

impl DacBackend for MockBackend {
    fn dac_type(&self) -> DacType {
        mock_dac_type()
    }

    fn caps(&self) -> &DacCapabilities {
        &MOCK_CAPS
    }

    fn connect(&mut self) -> Result<()> {
        self.connected = true;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    fn set_shutter(&mut self, _open: bool) -> Result<()> {
        Ok(())
    }
}

impl FifoBackend for MockBackend {
    fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
        Ok(WriteOutcome::Written)
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

/// A discoverer that always reports a single fixed device on prefix `smoke:`.
struct MockDiscoverer;

impl Discoverer for MockDiscoverer {
    fn dac_type(&self) -> DacType {
        mock_dac_type()
    }

    fn prefix(&self) -> &str {
        "smoke"
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let info = DiscoveredDeviceInfo::new(mock_dac_type(), "smoke:device-1", "Smoke Device 1")
            .with_hardware_name("Smoke Device 1");
        vec![DiscoveredDevice::new(
            info,
            Box::new(MockConnectData { _serial: 1 }),
        )]
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let _ = downcast_connect_data::<MockConnectData>(opaque, "SmokeDac")?;
        Ok(BackendKind::Fifo(Box::new(MockBackend {
            connected: false,
            estimator: SoftwareDecayEstimator::new(),
        })))
    }
}

/// A discovery factory that registers only the mock discoverer (no builtins).
fn mock_factory() -> DacDiscovery {
    let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
    discovery.register(Box::new(MockDiscoverer));
    discovery
}

// =============================================================================
// open_device_with
// =============================================================================

#[test]
fn open_device_with_finds_registered_custom_device_by_stable_id() {
    let dac = open_device_with("smoke:device-1", mock_factory)
        .expect("registered custom device should open");

    assert_eq!(dac.id(), "smoke:device-1");
    assert_eq!(dac.name(), "Smoke Device 1");
    assert_eq!(dac.kind(), &mock_dac_type());
    assert!(dac.has_backend(), "opened Dac should carry a live backend");
}

#[test]
fn open_device_with_returned_dac_carries_backend_caps() {
    let dac = open_device_with("smoke:device-1", mock_factory).expect("should open");

    // `open_discovered` overwrites the discoverer-derived caps with the caps
    // reported by the connected backend, so these must match MOCK_CAPS.
    let caps = &dac.info().caps;
    assert_eq!(caps.pps_min, MOCK_CAPS.pps_min);
    assert_eq!(caps.pps_max, MOCK_CAPS.pps_max);
    assert_eq!(caps.max_points_per_chunk, MOCK_CAPS.max_points_per_chunk);
    assert_eq!(caps.output_model, MOCK_CAPS.output_model);
    // The same caps are reachable through the convenience accessor.
    assert_eq!(dac.caps().pps_max, MOCK_CAPS.pps_max);
}

#[test]
fn open_device_with_unknown_id_on_known_prefix_errors() {
    // Prefix `smoke:` matches the discoverer, but no device has this id.
    let msg = match open_device_with("smoke:does-not-exist", mock_factory) {
        Ok(_) => panic!("unknown id should not open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("smoke:does-not-exist") || msg.to_lowercase().contains("not found"),
        "error should mention the missing device: {msg}"
    );
}

#[test]
fn open_device_with_unknown_prefix_errors_without_matching_discoverer() {
    // Prefix `ghost:` matches no registered discoverer -> fail fast.
    let msg = match open_device_with("ghost:whatever", mock_factory) {
        Ok(_) => panic!("unknown prefix should not open"),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.to_lowercase().contains("not found"),
        "error should indicate the DAC was not found: {msg}"
    );
}

// =============================================================================
// list_devices_filtered
// =============================================================================

#[test]
fn list_devices_filtered_with_none_enabled_returns_empty() {
    // No DAC types enabled -> no builtin discoverers registered -> no scan
    // touches real USB/network -> empty result. Fully hermetic.
    let devices = list_devices_filtered(&EnabledDacTypes::none()).expect("scan should succeed");
    assert!(
        devices.is_empty(),
        "no types enabled should yield no devices, got {}",
        devices.len()
    );
}
