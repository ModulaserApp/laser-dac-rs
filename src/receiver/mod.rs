//! IDN UDP receiver server.
//!
//! Hosts an IDN protocol UDP server that can accept incoming laser point
//! streams from IDN clients (e.g. TouchDesigner, `laser-dac` senders, or
//! the bundled `idn-simulator`). Consumers implement [`ServerBehavior`] to
//! react to discovery commands, frame data, and parsed point streams.
//!
//! Behind the `receiver` feature flag — disabled by default.
//!
//! # Example
//!
//! ```ignore
//! use laser_dac::receiver::{IdnServer, ServerConfig, ServerBehavior, ReceivedPoint};
//!
//! struct MyBehavior;
//!
//! impl ServerBehavior for MyBehavior {
//!     fn should_respond(&self, _command: u8) -> bool { true }
//!     fn get_status_byte(&self) -> u8 { 0x01 }
//!     fn get_ack_result_code(&self) -> u8 { 0x00 }
//!
//!     fn on_points_received(&mut self, points: &[ReceivedPoint]) {
//!         for p in points {
//!             println!("({}, {}) rgb=({}, {}, {})", p.x, p.y, p.r, p.g, p.b);
//!         }
//!     }
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     let config = ServerConfig::new("MyReceiver");
//!     let server = IdnServer::new(config, MyBehavior)?;
//!     let _handle = server.spawn();
//!     Ok(())
//! }
//! ```

mod behavior;
mod config;
mod constants;
mod packet_builder;
mod parser;
mod server;

pub use behavior::{ServerBehavior, SimpleBehavior};
pub use config::{Relay, ServerConfig, Service};
pub use constants::{
    IDNCMD_RT_CNLMSG, IDNCMD_RT_CNLMSG_CLOSE_ACKREQ, IDNCMD_SERVICE_PARAMS_REQUEST,
    IDNCMD_UNIT_PARAMS_REQUEST, IDNFLG_STATUS_EXCLUDED, IDNFLG_STATUS_MALFUNCTION,
    IDNFLG_STATUS_OCCUPIED, IDNFLG_STATUS_OFFLINE, IDNFLG_STATUS_REALTIME,
};
pub use parser::{parse_frame_data, ParsedChunk, ReceivedPoint, SampleFormat};
pub use server::{IdnServer, ServerHandle};
