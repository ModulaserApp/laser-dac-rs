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
//! For advanced frame-mode output-space processing on the final presented
//! sequence, use [`FrameSessionConfig::with_output_filter`]. The filter runs
//! after transition composition, blanking, and color delay, just before the
//! backend write.
//!
//! For downstream watchdogs, [`FrameSession::metrics`] exposes a small liveness
//! surface with connectivity, last-loop-activity, and last-write-success
//! timestamps. Watchdog policy remains application-owned.
//!
//! ## Callback Mode (advanced, FIFO backends only)
//!
//! Fill point buffers via zero-allocation callback for custom timing.
//! Not available for frame-swap backends (Helios) — use Frame Mode instead.
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
//! - **Helios** - USB laser DAC, Frame API only (feature: `helios`)
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
pub(crate) mod reconnect;
mod scheduler;
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
    IdlePolicy,
    LaserPoint,
    OutputModel,
    ReconnectConfig,
    RunExit,
    StreamConfig,
    StreamInstant,
    StreamStats,
    StreamStatus,
};

// Deprecated alias for backwards compatibility
#[allow(deprecated)]
pub use types::UnderrunPolicy;

// Stream and Dac types
pub use stream::{Dac, Stream, StreamControl};

// Presentation types (frame-first API)
pub use presentation::{
    default_transition, Frame, FrameSession, FrameSessionConfig, FrameSessionMetrics, OutputFilter,
    OutputFilterContext, OutputResetReason, PresentedSliceKind, TransitionFn, TransitionPlan,
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
    let mut dac = discovery.open_by_id(id)?;
    dac.reconnect_target = Some(reconnect::ReconnectTarget {
        device_id: id.to_string(),
        discovery_factory: None,
    });
    Ok(dac)
}

/// Open a DAC by ID using a custom discovery factory.
///
/// Like [`open_device`], but uses the provided factory to create the
/// [`DacDiscovery`] instance. This is required for custom backends
/// registered via [`DacDiscovery::register`] — the default `open_device`
/// only finds built-in DAC types.
///
/// The factory is called once now for the initial open, and stored for
/// future reconnection attempts. It must be `Fn` (not `FnOnce`) because
/// reconnection may call it multiple times.
///
/// # Example
///
/// ```ignore
/// use laser_dac::{open_device_with, DacDiscovery, EnabledDacTypes, FrameSessionConfig, ReconnectConfig};
///
/// let dac = open_device_with("shownet:my-device", || {
///     let mut d = DacDiscovery::new(EnabledDacTypes::all());
///     d.register(Box::new(MyShowNetDiscoverer::new()));
///     d
/// })?;
///
/// let config = FrameSessionConfig::new(30_000)
///     .with_reconnect(ReconnectConfig::new());
/// let (session, _info) = dac.start_frame_session(config)?;
/// // Reconnection will also use the factory to find the custom backend
/// ```
pub fn open_device_with<F>(id: &str, factory: F) -> BackendResult<Dac>
where
    F: Fn() -> DacDiscovery + Send + 'static,
{
    let mut discovery = factory();
    let mut dac = discovery.open_by_id(id)?;
    dac.reconnect_target = Some(reconnect::ReconnectTarget {
        device_id: id.to_string(),
        discovery_factory: Some(Box::new(factory)),
    });
    Ok(dac)
}
