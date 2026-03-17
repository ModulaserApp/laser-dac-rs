//! Unified DAC backend abstraction for laser projectors.
//!
//! This crate provides a common interface for communicating with various
//! laser DAC (Digital-to-Analog Converter) hardware. Two API styles are
//! available:
//!
//! # Getting Started
//!
//! ## Frame Mode (recommended)
//!
//! Submit complete frames with automatic transition blanking:
//!
//! ```no_run
//! use laser_dac::{open_device, FrameSessionConfig, Frame, LaserPoint};
//!
//! let device = open_device("my-device").unwrap();
//! let config = FrameSessionConfig::new(30_000);
//! let (session, _info) = device.start_frame_session(config).unwrap();
//!
//! session.control().arm().unwrap();
//! session.send_frame(Frame::new(vec![
//!     LaserPoint::new(-0.5, 0.0, 65535, 0, 0, 65535),
//!     LaserPoint::new( 0.5, 0.0, 0, 65535, 0, 65535),
//! ]));
//! // Frame replays automatically. Submit new frames for animation.
//! ```
//!
//! ## Callback Mode (advanced)
//!
//! Fill point buffers via zero-allocation callback for custom timing:
//!
//! ```no_run
//! use laser_dac::{open_device, StreamConfig, LaserPoint, ChunkRequest, ChunkResult};
//!
//! let device = open_device("my-device").unwrap();
//! let config = StreamConfig::new(30_000);
//! let (stream, _info) = device.start_stream(config).unwrap();
//!
//! stream.control().arm().unwrap();
//!
//! let exit = stream.run(
//!     |req: &ChunkRequest, buffer: &mut [LaserPoint]| {
//!         let n = req.target_points;
//!         for i in 0..n {
//!             buffer[i] = LaserPoint::blanked(0.0, 0.0);
//!         }
//!         ChunkResult::Filled(n)
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
//! - **AVB Audio Devices** - AVB audio output via CoreAudio/ASIO (feature: `avb`, macOS/Windows)
//!
//! # Features
//!
//! - `all-dacs` (default): Enable all DAC protocols
//! - `usb-dacs`: Enable USB DACs (Helios, LaserCube USB)
//! - `network-dacs`: Enable network DACs (Ether Dream, IDN, LaserCube WiFi)
//! - `audio-dacs`: Enable audio DACs (AVB)
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
#[cfg(any(feature = "idn", feature = "lasercube-wifi"))]
mod net_utils;
pub mod presentation;
pub mod protocols;
pub mod session;
pub mod stream;
pub mod types;

// Crate-level error types
pub use error::{Error, Result};

// Backend traits and types
pub use backend::{BackendKind, DacBackend, FifoBackend, FrameSwapBackend, WriteOutcome};

// Discovery types
pub use discovery::{
    DacDiscovery, DiscoveredDevice, DiscoveredDeviceInfo, ExternalDevice, ExternalDiscoverer,
};

// Core types
pub use types::{
    // DAC types
    caps_for_dac_type,
    ChunkRequest,
    ChunkResult,
    // Streaming types
    DacCapabilities,
    DacConnectionState,
    DacDevice,
    DacInfo,
    DacType,
    EnabledDacTypes,
    LaserPoint,
    OutputModel,
    RunExit,
    StreamConfig,
    StreamInstant,
    StreamStats,
    StreamStatus,
    UnderrunPolicy,
};

// Stream and Dac types
pub use session::{FrameSessionHandle, ReconnectingSession, SessionControl};
pub use stream::{Dac, Stream, StreamControl};

// Presentation types (frame-first API)
pub use presentation::{
    default_transition, Frame, FrameSession, FrameSessionConfig, TransitionFn,
};

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

// AVB
#[cfg(feature = "avb")]
pub use backend::AvbBackend;
#[cfg(feature = "avb")]
pub use protocols::avb;

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
/// `idn:hostname.local`, `helios:serial`, `avb:device-slug:n`).
pub fn open_device(id: &str) -> BackendResult<Dac> {
    let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
    discovery.open_by_id(id)
}
