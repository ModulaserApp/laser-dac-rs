//! IDN protocol constants.

// Command codes
pub const IDNCMD_PING_REQUEST: u8 = 0x08;
pub const IDNCMD_PING_RESPONSE: u8 = 0x09;
pub const IDNCMD_SCAN_REQUEST: u8 = 0x10;
pub const IDNCMD_SCAN_RESPONSE: u8 = 0x11;
pub const IDNCMD_SERVICEMAP_REQUEST: u8 = 0x12;
pub const IDNCMD_SERVICEMAP_RESPONSE: u8 = 0x13;
pub const IDNCMD_RT_CNLMSG: u8 = 0x40;
pub const IDNCMD_RT_CNLMSG_ACKREQ: u8 = 0x41;
pub const IDNCMD_RT_CNLMSG_CLOSE: u8 = 0x44;
pub const IDNCMD_RT_CNLMSG_CLOSE_ACKREQ: u8 = 0x45;
pub const IDNCMD_RT_ACK: u8 = 0x47;

// Parameter commands
pub const IDNCMD_UNIT_PARAMS_REQUEST: u8 = 0x22;
pub const IDNCMD_UNIT_PARAMS_RESPONSE: u8 = 0x23;
pub const IDNCMD_SERVICE_PARAMS_REQUEST: u8 = 0x20;
pub const IDNCMD_SERVICE_PARAMS_RESPONSE: u8 = 0x21;

// Service types
pub const IDNVAL_STYPE_LAPRO: u8 = 0x80;
pub const IDNVAL_STYPE_DMX512: u8 = 0x05;

// Service map flags
pub const IDNFLG_SERVICEMAP_DSID: u8 = 0x01;

// Status byte flags (for scan response)
pub const IDNFLG_STATUS_MALFUNCTION: u8 = 0x80;
pub const IDNFLG_STATUS_OFFLINE: u8 = 0x40;
pub const IDNFLG_STATUS_EXCLUDED: u8 = 0x20;
pub const IDNFLG_STATUS_OCCUPIED: u8 = 0x10;
pub const IDNFLG_STATUS_REALTIME: u8 = 0x01;

// Default port
pub const IDN_PORT: u16 = 7255;
