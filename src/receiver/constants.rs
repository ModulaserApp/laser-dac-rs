//! IDN protocol constants used by the receiver server.
//!
//! These re-export the canonical constants from
//! [`crate::protocols::idn::protocol`] under the names historically used by
//! the receiver/mock server, so existing consumers don't need to learn two
//! sets of names.

use crate::protocols::idn::protocol;

// Command codes
pub use protocol::IDNCMD_RT_ACKNOWLEDGE as IDNCMD_RT_ACK;
pub use protocol::{
    IDNCMD_PING_REQUEST, IDNCMD_PING_RESPONSE, IDNCMD_RT_CNLMSG, IDNCMD_RT_CNLMSG_ACKREQ,
    IDNCMD_RT_CNLMSG_CLOSE, IDNCMD_RT_CNLMSG_CLOSE_ACKREQ, IDNCMD_SCAN_REQUEST,
    IDNCMD_SCAN_RESPONSE, IDNCMD_SERVICEMAP_REQUEST, IDNCMD_SERVICEMAP_RESPONSE,
    IDNCMD_SERVICE_PARAMS_REQUEST, IDNCMD_SERVICE_PARAMS_RESPONSE, IDNCMD_UNIT_PARAMS_REQUEST,
    IDNCMD_UNIT_PARAMS_RESPONSE,
};

// Service types
pub use protocol::{IDNVAL_STYPE_DMX512, IDNVAL_STYPE_LAPRO};

// RT-ACK result codes
#[allow(unused_imports)]
pub use protocol::{
    IDNVAL_RTACK_ERR_EMPTY_CLOSE, IDNVAL_RTACK_ERR_EXCLUDED, IDNVAL_RTACK_ERR_INVALID_PAYLOAD,
    IDNVAL_RTACK_ERR_OCCUPIED, IDNVAL_RTACK_ERR_PROCESSING_ERROR,
};

// Service map flags
pub use protocol::IDNFLG_SERVICEMAP_DSID;

// Default port
pub use protocol::IDN_PORT;

// Status byte flags (for scan response).
// The protocol module spells these IDNFLG_SCAN_STATUS_*; receiver consumers
// historically use the shorter IDNFLG_STATUS_* names.
pub use protocol::IDNFLG_SCAN_STATUS_EXCLUDED as IDNFLG_STATUS_EXCLUDED;
pub use protocol::IDNFLG_SCAN_STATUS_MALFUNCTION as IDNFLG_STATUS_MALFUNCTION;
pub use protocol::IDNFLG_SCAN_STATUS_OCCUPIED as IDNFLG_STATUS_OCCUPIED;
pub use protocol::IDNFLG_SCAN_STATUS_OFFLINE as IDNFLG_STATUS_OFFLINE;
pub use protocol::IDNFLG_SCAN_STATUS_REALTIME as IDNFLG_STATUS_REALTIME;
