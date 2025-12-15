//! Unified DAC backend abstraction for laser projectors.
//!
//! This crate provides a common interface for communicating with various
//! laser DAC (Digital-to-Analog Converter) hardware, allowing you to write
//! laser frames without worrying about device-specific protocols.
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
//! - Colors: 0-255 for R, G, B, and intensity
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
pub use discovery_worker::DacDiscoveryWorker;
pub use worker::DacWorker;

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
