//! Backwards-compatible re-exports.
//!
//! The types previously defined here have moved to focused modules:
//!
//! - [`LaserPoint`] → [`crate::point`]
//! - [`DacType`], [`DacCapabilities`], [`DacInfo`], [`EnabledDacTypes`], … →
//!   [`crate::device`]
//! - [`StreamConfig`], [`IdlePolicy`], [`ReconnectConfig`] → [`crate::config`]
//! - [`StreamInstant`], [`ChunkRequest`], [`ChunkResult`], [`StreamStatus`],
//!   [`StreamStats`], [`RunExit`] → [`crate::stream`]
//!
//! This module re-exports them so existing `laser_dac::types::Foo` paths keep
//! working. Prefer the new module paths in new code.

#[allow(deprecated)]
pub use crate::config::UnderrunPolicy;
pub use crate::config::{IdlePolicy, ReconnectConfig, StreamConfig};
pub use crate::device::{
    caps_for_dac_type, DacCapabilities, DacConnectionState, DacDevice, DacInfo, DacType,
    EnabledDacTypes, OutputModel,
};
pub use crate::point::LaserPoint;
pub use crate::stream::{
    ChunkRequest, ChunkResult, RunExit, StreamInstant, StreamStats, StreamStatus,
};
