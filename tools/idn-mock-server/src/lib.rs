//! Mock IDN protocol server for testing and simulation.
//!
//! This crate provides a reusable mock IDN server that can be customized via the
//! [`ServerBehavior`] trait. It is used by:
//! - Test infrastructure for end-to-end protocol testing
//! - The visual IDN simulator for debugging without hardware
//!
//! # Architecture
//!
//! The crate is organized into layers:
//! - **Protocol layer**: Constants and packet builders (stateless)
//! - **Configuration layer**: Service/relay configuration types
//! - **Server layer**: Core UDP server with trait-based behavior hooks
//!
//! # Example
//!
//! ```ignore
//! use idn_mock_server::{MockIdnServer, ServerConfig, ServerBehavior};
//!
//! struct MyBehavior;
//!
//! impl ServerBehavior for MyBehavior {
//!     fn on_frame_received(&mut self, _raw_data: &[u8]) {}
//!     fn should_respond(&self, _command: u8) -> bool { true }
//!     fn get_status_byte(&self) -> u8 { 0x01 }
//!     fn get_ack_result_code(&self) -> u8 { 0x00 }
//! }
//!
//! let config = ServerConfig::new("TestServer");
//! let server = MockIdnServer::new(config, MyBehavior)?;
//! let handle = server.spawn();
//! ```

mod behavior;
mod config;
mod constants;
mod packet_builder;
mod server;

pub use behavior::ServerBehavior;
pub use config::{MockRelay, MockService, ServerConfig};
pub use constants::*;
pub use packet_builder::*;
pub use server::{MockIdnServer, ServerHandle};
