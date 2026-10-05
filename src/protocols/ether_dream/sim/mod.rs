//! An Ether Dream simulator for tests and tools.
//!
//! Available with the `testutils` feature (and always in this crate's own unit
//! tests). It is not part of the stable API and may change in any release.
//!
//! * [`EtherDreamModel`] is the protocol state machine. It takes bytes and an
//!   injected clock and returns replies, with no sockets involved.
//! * [`SimServer`] wraps a model in a TCP command server and a UDP broadcaster,
//!   with knobs for injecting network faults.
//!
//! Both are driven by a [`FirmwareProfile`](super::FirmwareProfile).

pub mod model;
pub mod server;

pub use model::{EtherDreamModel, Reply};
pub use server::{Faults, SimServer, SimServerConfig};
