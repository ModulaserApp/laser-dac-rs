//! Protocol implementations for various laser DAC types.
//!
//! This module contains the low-level protocol implementations for each
//! supported DAC type. Each protocol is gated behind a feature flag.

/// Shared USB endpoint seam used by the USB-based backends (Helios, LaserCube
/// USB) so their transfer logic can be tested against a fake device.
#[cfg(any(feature = "helios", feature = "lasercube-usb"))]
pub mod usb_transfer;

#[cfg(feature = "helios")]
pub mod helios;

#[cfg(feature = "ether-dream")]
pub mod ether_dream;

#[cfg(feature = "idn")]
pub mod idn;

#[cfg(feature = "lasercube-network")]
pub mod lasercube_network;

#[cfg(feature = "lasercube-usb")]
pub mod lasercube_usb;

#[cfg(feature = "oscilloscope")]
pub mod oscilloscope;

#[cfg(feature = "avb")]
pub mod avb;
