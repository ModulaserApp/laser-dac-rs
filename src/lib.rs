//! Unified DAC backend abstraction for laser projectors.
//!
//! This crate provides a common interface for communicating with various
//! laser DAC (Digital-to-Analog Converter) hardware using a streaming API
//! that provides uniform pacing and backpressure across all device types.
//!
//! # Getting Started
//!
//! The streaming API provides two modes of operation:
//!
//! ## Blocking Mode
//!
//! Use `next_request()` to get what to produce, then `write()` to send points:
//!
//! ```no_run
//! use laser_dac::{list_devices, open_device, StreamConfig, LaserPoint};
//!
//! // Discover devices
//! let devices = list_devices().unwrap();
//! println!("Found {} devices", devices.len());
//!
//! // Open and start streaming
//! let device = open_device(&devices[0].id).unwrap();
//! let config = StreamConfig::new(30_000); // 30k points per second
//! let (mut stream, info) = device.start_stream(config).unwrap();
//!
//! // Arm the output (allow laser to fire)
//! stream.control().arm().unwrap();
//!
//! // Streaming loop
//! loop {
//!     let req = stream.next_request().unwrap();
//!
//!     // Generate points for this chunk
//!     let points: Vec<LaserPoint> = (0..req.n_points)
//!         .map(|i| {
//!             let t = (req.start.points() + i as u64) as f32 / req.pps as f32;
//!             let angle = t * std::f32::consts::TAU;
//!             LaserPoint::new(angle.cos(), angle.sin(), 65535, 0, 0, 65535)
//!         })
//!         .collect();
//!
//!     stream.write(&req, &points).unwrap();
//! }
//! ```
//!
//! ## Callback Mode
//!
//! Use `run()` with a producer closure for simpler code. The producer receives
//! a pre-allocated buffer to fill, eliminating per-chunk heap allocation:
//!
//! ```no_run
//! use laser_dac::{list_devices, open_device, StreamConfig, LaserPoint, ChunkRequest, ProducerResult};
//!
//! let device = open_device("my-device").unwrap();
//! let config = StreamConfig::new(30_000);
//! let (stream, _info) = device.start_stream(config).unwrap();
//!
//! stream.control().arm().unwrap();
//!
//! let exit = stream.run(
//!     |req: ChunkRequest, buffer: &mut [LaserPoint]| {
//!         // Fill the buffer with points (pre-filled with blanks)
//!         for (i, point) in buffer.iter_mut().enumerate() {
//!             let t = (req.start.points() + i as u64) as f32 / req.pps as f32;
//!             let angle = t * std::f32::consts::TAU;
//!             *point = LaserPoint::new(angle.cos(), angle.sin(), 65535, 0, 0, 65535);
//!         }
//!         ProducerResult::Continue // or ProducerResult::End to stop
//!     },
//!     |err| eprintln!("Stream error: {}", err),
//! );
//! ```
//!
//! # Supported DACs
//!
//! - **Helios** - USB laser DAC (feature: `helios`)
//! - **Ether Dream** - Network laser DAC (feature: `ether-dream`)
//! - **IDN** - ILDA Digital Network protocol (feature: `idn`)
//! - **LaserCube WiFi** - WiFi-connected laser DAC (feature: `lasercube-wifi`)
//! - **LaserCube USB** - USB laser DAC / LaserDock (feature: `lasercube-usb`)
//!
//! # Features
//!
//! - `all-dacs` (default): Enable all DAC protocols
//! - `usb-dacs`: Enable USB DACs (Helios, LaserCube USB)
//! - `network-dacs`: Enable network DACs (Ether Dream, IDN, LaserCube WiFi)
//!
//! # Coordinate System
//!
//! All backends use normalized coordinates:
//! - X: -1.0 (left) to 1.0 (right)
//! - Y: -1.0 (bottom) to 1.0 (top)
//! - Colors: 0-65535 for R, G, B, and intensity
//!
//! Each backend handles conversion to its native format internally.

pub mod backend;
pub mod discovery;
mod error;
mod frame_adapter;
pub mod protocols;
pub mod session;
pub mod stream;
pub mod types;

// Crate-level error types
pub use error::{Error, Result};

// Backend trait and types
pub use backend::{StreamBackend, WriteOutcome};

// Discovery types
pub use discovery::{
    DacDiscovery, DiscoveredDevice, DiscoveredDeviceInfo, ExternalDevice, ExternalDiscoverer,
};

// Core types
pub use types::{
    // DAC types
    caps_for_dac_type,
    ChunkRequest,
    // Streaming types
    DacCapabilities,
    DacConnectionState,
    DacDevice,
    DacInfo,
    DacType,
    EnabledDacTypes,
    LaserPoint,
    OutputModel,
    ProducerResult,
    RunExit,
    StreamConfig,
    StreamInstant,
    StreamStats,
    StreamStatus,
    UnderrunPolicy,
};

// Stream and Dac types
pub use session::{ReconnectingSession, SessionControl};
pub use stream::{Dac, OwnedDac, Stream, StreamControl};

// Frame adapters (converts point buffers to continuous streams)
pub use frame_adapter::{Frame, FrameAdapter, SharedFrameAdapter};

// Conditional exports based on features

// Helios
#[cfg(feature = "helios")]
pub use backend::HeliosBackend;
#[cfg(feature = "helios")]
pub use protocols::helios;

// Ether Dream
#[cfg(feature = "ether-dream")]
pub use backend::EtherDreamBackend;
#[cfg(feature = "ether-dream")]
pub use protocols::ether_dream;

// IDN
#[cfg(feature = "idn")]
pub use backend::IdnBackend;
#[cfg(feature = "idn")]
pub use protocols::idn;

// LaserCube WiFi
#[cfg(feature = "lasercube-wifi")]
pub use backend::LasercubeWifiBackend;
#[cfg(feature = "lasercube-wifi")]
pub use protocols::lasercube_wifi;

// LaserCube USB
#[cfg(feature = "lasercube-usb")]
pub use backend::LasercubeUsbBackend;
#[cfg(feature = "lasercube-usb")]
pub use protocols::lasercube_usb;

// Re-export rusb for consumers that need the Context type (for LaserCube USB)
#[cfg(feature = "lasercube-usb")]
pub use protocols::lasercube_usb::rusb;

// =============================================================================
// Device Discovery Functions
// =============================================================================

use backend::Result as BackendResult;

/// List all available DACs.
///
/// Returns DAC info for each discovered DAC, including capabilities.
pub fn list_devices() -> BackendResult<Vec<DacInfo>> {
    list_devices_filtered(&EnabledDacTypes::all())
}

/// List available DACs filtered by DAC type.
pub fn list_devices_filtered(enabled_types: &EnabledDacTypes) -> BackendResult<Vec<DacInfo>> {
    let mut discovery = DacDiscovery::new(enabled_types.clone());
    let devices = discovery
        .scan()
        .into_iter()
        .map(|device| {
            let info = device.info();
            DacInfo {
                id: info.stable_id(),
                name: info.name(),
                kind: device.dac_type(),
                caps: caps_for_dac_type(&device.dac_type()),
            }
        })
        .collect();

    Ok(devices)
}

/// Open a DAC by ID.
///
/// The ID should match the `id` field returned by [`list_devices`].
/// IDs are namespaced by protocol (e.g., `etherdream:aa:bb:cc:dd:ee:ff`,
/// `idn:hostname.local`, `helios:serial`).
pub fn open_device(id: &str) -> BackendResult<Dac> {
    let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
    let discovered = discovery.scan();

    let device = discovered
        .into_iter()
        .find(|d| d.info().stable_id() == id)
        .ok_or_else(|| backend::Error::disconnected(format!("DAC not found: {}", id)))?;

    let info = device.info();
    let name = info.name();
    let dac_type = device.dac_type();
    let stream_backend = discovery.connect(device)?;

    let dac_info = DacInfo {
        id: id.to_string(),
        name,
        kind: dac_type,
        caps: stream_backend.caps().clone(),
    };

    Ok(Dac::new(dac_info, stream_backend))
}
