//! Behavior trait for customizing receiver server responses.

use std::net::SocketAddr;
use std::time::Duration;

use super::constants::IDNFLG_STATUS_REALTIME;
use super::parser::ReceivedPoint;

/// Behavior hooks for customizing receiver server responses.
///
/// Implement this trait to control how the server responds to various events
/// and commands, and to consume incoming point streams. The same server
/// core can therefore be used for:
/// - Test infrastructure (simple flags, packet capture)
/// - Visual simulators (UI-driven settings, full frame parsing)
/// - Production receivers (consume parsed points directly)
pub trait ServerBehavior: Send + 'static {
    /// Called for every packet received by the server.
    ///
    /// This fires before any command processing — useful when you want to
    /// capture all traffic regardless of command type.
    fn on_packet_received(&mut self, _raw_data: &[u8]) {}

    /// Called when raw frame data is received (RT_CNLMSG or RT_CNLMSG_ACKREQ).
    ///
    /// The raw packet data includes the full packet starting from the command
    /// byte. Production consumers should normally rely on
    /// [`Self::on_points_received`] instead; this raw hook exists for tools
    /// (like the simulator) that want byte-level inspection.
    fn on_frame_received(&mut self, _raw_data: &[u8]) {}

    /// Called once per channel message with the parsed points.
    ///
    /// Points are normalized (`x`, `y` in `-1.0..=1.0`; `r`, `g`, `b` in
    /// `0.0..=1.0`) with intensity already pre-multiplied into RGB. Frames
    /// that use an unsupported sample format are skipped and don't fire this
    /// callback.
    fn on_points_received(&mut self, _points: &[ReceivedPoint]) {}

    /// Whether to respond to a given command.
    ///
    /// Return `false` to simulate offline/silent mode where the server
    /// receives packets but doesn't respond.
    fn should_respond(&self, command: u8) -> bool;

    /// Get the status byte for scan responses.
    ///
    /// Combine `IDNFLG_STATUS_*` flags.
    fn get_status_byte(&self) -> u8;

    /// Get the ACK result code for frame acknowledgments.
    ///
    /// Return `0x00` for success, or an error code like:
    /// - `0xEB` - Empty close
    /// - `0xEC` - Sessions occupied
    /// - `0xED` - Group excluded
    /// - `0xEE` - Invalid payload
    /// - `0xEF` - Processing error
    fn get_ack_result_code(&self) -> u8;

    /// Optional pre-processing latency to simulate network delay.
    fn get_simulated_latency(&self) -> Duration {
        Duration::ZERO
    }

    /// Called when a client connects (first frame received from new address).
    fn on_client_connected(&mut self, _addr: SocketAddr) {}

    /// Called when a client disconnects (timeout or force disconnect).
    fn on_client_disconnected(&mut self) {}

    /// Check if the current client should be force-disconnected.
    ///
    /// If this returns `true`, the server will:
    /// 1. Disconnect the current client
    /// 2. Ignore packets from them for 3 seconds
    /// 3. Call [`Self::on_client_disconnected`]
    ///
    /// Implementations should clear any "force disconnect" flag when returning `true`.
    fn should_force_disconnect(&mut self) -> bool {
        false
    }

    /// Whether to reject a new client when one is already connected.
    fn is_occupied(&self) -> bool {
        false
    }

    /// Whether to reject all real-time messages with an "excluded" error.
    fn is_excluded(&self) -> bool {
        false
    }
}

/// A simple behavior implementation for basic testing.
///
/// - Always responds to commands
/// - Reports REALTIME capability in status
/// - Returns success for ACK responses
/// - Ignores frame data and connection events
#[derive(Default)]
pub struct SimpleBehavior;

impl ServerBehavior for SimpleBehavior {
    fn should_respond(&self, _command: u8) -> bool {
        true
    }

    fn get_status_byte(&self) -> u8 {
        IDNFLG_STATUS_REALTIME
    }

    fn get_ack_result_code(&self) -> u8 {
        0x00
    }
}
