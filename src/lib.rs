//! Unified DAC backend abstraction for laser projectors.
//!
//! This crate provides a common interface for communicating with various
//! laser DAC (Digital-to-Analog Converter) hardware, allowing you to write
//! laser frames without worrying about device-specific protocols.
//!
//! # Getting Started
//!
//! The easiest way to use this crate is with [`DacDiscoveryWorker`], which handles
//! device discovery and connection in the background:
//!
//! ```no_run
//! use laser_dac::{DacDiscoveryWorker, EnabledDacTypes, LaserFrame, LaserPoint};
//! use std::thread;
//! use std::time::Duration;
//!
//! // Start background discovery for all DAC types
//! let discovery = DacDiscoveryWorker::builder()
//!     .enabled_types(EnabledDacTypes::all())
//!     .build();
//!
//! // Collect discovered devices
//! let mut workers = Vec::new();
//! for _ in 0..50 {
//!     for worker in discovery.poll_new_workers() {
//!         println!("Found: {} ({})", worker.device_name(), worker.dac_type());
//!         workers.push(worker);
//!     }
//!     thread::sleep(Duration::from_millis(100));
//! }
//!
//! // Create a frame (simple square)
//! let points = vec![
//!     LaserPoint::blanked(-0.5, -0.5),
//!     LaserPoint::new(-0.5, -0.5, 65535, 0, 0, 65535),
//!     LaserPoint::new(0.5, -0.5, 0, 65535, 0, 65535),
//!     LaserPoint::new(0.5, 0.5, 0, 0, 65535, 65535),
//!     LaserPoint::new(-0.5, 0.5, 65535, 65535, 0, 65535),
//!     LaserPoint::new(-0.5, -0.5, 65535, 0, 0, 65535),
//! ];
//! let frame = LaserFrame::new(30000, points);
//!
//! // Send frames to all connected DACs
//! loop {
//!     for worker in &mut workers {
//!         worker.update();
//!         worker.submit_frame(frame.clone());
//!     }
//!     thread::sleep(Duration::from_millis(33));
//! }
//! ```
//!
//! For more control over discovery and connection, use [`DacDiscovery`] directly:
//!
//! ```no_run
//! use laser_dac::{DacDiscovery, DacWorker, EnabledDacTypes, LaserFrame, LaserPoint};
//!
//! let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
//! let devices = discovery.scan();
//!
//! for device in devices {
//!     let name = device.name().to_string();
//!     let dac_type = device.dac_type();
//!     let backend = discovery.connect(device).expect("connection failed");
//!     let mut worker = DacWorker::new(name, dac_type, backend);
//!
//!     // Use worker.update() and worker.submit_frame() in your render loop
//! }
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
pub mod discovery_worker;
mod error;
pub mod protocols;
pub mod types;
pub mod worker;

// Error types
pub use error::{Error, Result};

// Backend trait and common types
pub use backend::{DacBackend, WriteResult};

// Discovery and worker types
pub use discovery::{CustomDiscoverySource, DacDiscovery, DiscoveredDevice, DiscoveredDeviceInfo};
pub use discovery_worker::{DacDiscoveryWorker, DacDiscoveryWorkerBuilder};
pub use worker::{
    CallbackContext, CallbackError, DacCallbackWorker, DacWorker, WorkerCommand, WorkerStatus,
};

// Types
pub use types::{
    DacConnectionState, DacDevice, DacType, DiscoveredDac, EnabledDacTypes, LaserFrame, LaserPoint,
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

// Re-export rusb for consumers that need the Context type (for LaserCube USB)
#[cfg(feature = "lasercube-usb")]
pub use protocols::lasercube_usb::rusb;
