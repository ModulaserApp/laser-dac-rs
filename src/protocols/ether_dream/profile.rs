//! Firmware profiles: what an Ether Dream advertises and how it behaves.
//!
//! Ether Dream hardware generations (ED1 through ED4) speak the same wire
//! protocol but differ in buffer size, advertised flags and a handful of edge
//! cases (whether NAK-Full is ever sent, what happens to a `begin` while idle,
//! and so on). A [`FirmwareProfile`] records both halves:
//!
//! * **Advertised parameters**: the values a DAC reports in its UDP broadcast
//!   and status frames (capacity, max rate, revisions, always-set flag bits,
//!   the `'v'` build string).
//! * **Behaviour flags**: observable reactions to specific command sequences.
//!
//! The backend never branches on a profile. It reacts to what the DAC reports.
//! Profiles exist so the simulator (the `sim` module, behind the `testutils`
//! feature) can reproduce each firmware, so tests can be parametrised over
//! [`FirmwareProfile::all`], and so a `'v'` reply can be mapped to a
//! human-readable name for logging via [`FirmwareProfile::identify`].
//!
//! Every preset carries a [`Provenance`] that says how much of it is known.

/// How a profile's facts were established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Provenance {
    /// Probed against real hardware running this firmware.
    HardwareVerified,
    /// Read from the published firmware source code, not probed on hardware.
    FirmwareSource,
    /// Guessed. Treat every value as a placeholder until someone captures a
    /// trace from real hardware (see `tests/fixtures/ether_dream/README.md`).
    Unverified,
}

/// Advertised parameters and observed behaviour of one Ether Dream firmware.
///
/// Fields are public so tests and the simulator can tweak a preset:
///
/// ```
/// use laser_dac::protocols::ether_dream::FirmwareProfile;
///
/// let mut p = FirmwareProfile::ed2_r331();
/// p.naks_full = true; // pretend this firmware rejects overfull writes
/// assert_eq!(p.buffer_capacity, 3899);
/// ```
///
/// New fields may be added in minor releases, so construct profiles from a
/// preset rather than with a struct literal.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FirmwareProfile {
    /// Short identifier, e.g. `"ed2-r331"`.
    pub name: &'static str,
    /// How the facts in this profile were established.
    pub provenance: Provenance,
    /// Free-form notes on what is assumed and why.
    pub notes: &'static str,

    // ---- advertised parameters ---------------------------------------------
    /// MAC address reported in the broadcast.
    pub mac_address: [u8; 6],
    /// Hardware revision reported in the broadcast.
    pub hw_revision: u16,
    /// Software revision reported in the broadcast.
    pub sw_revision: u16,
    /// Point capacity reported in the broadcast.
    pub buffer_capacity: u16,
    /// Maximum point rate reported in the broadcast.
    pub max_point_rate: u32,
    /// Build string returned by the `'v'` command, if the firmware answers it.
    ///
    /// ED2 r331 answers with exactly 32 raw bytes: the build string padded
    /// with NULs, with no response byte or command echo in front.
    pub version_string: Option<&'static str>,
    /// Playback-flag bits the firmware always sets in every status frame, on
    /// top of the documented shutter/underflow/e-stop bits. ED2 r331 reported
    /// `0x5f31` while idle in the first fixture captures, so its mask is
    /// `0x5f31`. The undocumented upper byte is not stable on real hardware:
    /// the same DAC reported `0x7731` after a power cycle. Clients must
    /// ignore it.
    pub always_set_playback_flags: u16,
    /// Points the ring buffer really holds. Can exceed `buffer_capacity` in a
    /// simulated profile. On ED2 r331 both are 3899: the ring accepted exactly
    /// the advertised capacity, and the next write got NAK-Invalid.
    pub ring_points: u16,

    // ---- behaviour flags ---------------------------------------------------
    /// A `'d'` that does not fit is rejected whole with NAK-Full.
    /// When false, see [`Self::data_partial_write_then_nak_invalid`].
    pub naks_full: bool,
    /// `'b'` while idle is ACKed and silently ignored (true) or NAK-Invalid.
    pub begin_while_idle_acked: bool,
    /// `'s'` while idle answers NAK-Invalid (true) or ACK.
    pub stop_while_idle_naks: bool,
    /// After `'s'`, estop or underflow-free stop, buffer fullness keeps its old
    /// value until the next `'p'` (true). When false the ring is cleared.
    pub stale_fullness_until_prepare: bool,
    /// `'c'` returns the light engine to Ready at once (true). When false it
    /// passes through Warmup for about 100 ms first, and the `'c'` is NAKed
    /// with `'!'` because, as in j4cDAC, only a clear that leaves the light
    /// engine Ready is ACKed.
    pub estop_clear_immediate: bool,
    /// An unknown command byte closes the TCP connection (true). When false it
    /// is answered with NAK-Invalid and one byte is consumed.
    pub unknown_command_resets_tcp: bool,
    /// When `naks_full` is false: an overfull `'d'` writes what fits, swallows
    /// the rest and replies NAK-Invalid (true). When false, nothing is written
    /// and the reply is NAK-Invalid.
    pub data_partial_write_then_nak_invalid: bool,
    /// `'d'` while idle or underflowed answers NAK-Invalid (true). When false
    /// the points are dropped and the command is ACKed.
    pub data_while_idle_nak_invalid: bool,
    /// Only one TCP client may be connected at a time.
    ///
    /// ED2 r331 accepts more than one, and every client drives the same
    /// playback state: a `'p'`, `'s'` or e-stop from one client acts on the
    /// stream another client is playing.
    pub enforce_single_client: bool,
    /// A `'b'`, `'u'` or `'q'` with an out-of-range rate is NAKed after
    /// consuming only the opcode byte (true), so the remaining argument bytes
    /// are parsed as further commands. When false the whole command is consumed.
    ///
    /// Observed on ED2 r331: sending `'b'` with all six argument bytes set to
    /// `'?'` (a rate above max) produced the NAK plus one ping reply per
    /// argument byte. By the same rule, a low-water mark of 0 is read as two
    /// `0x00` bytes, which are e-stop commands. Never send such a rate.
    pub rate_nak_consumes_one_byte: bool,
    /// A `'b'`, `'u'` or `'q'` with rate 0 hangs the firmware (true) instead
    /// of being NAKed. j4cDAC asserts on it for `'b'` and `'u'`, and ED2 r331
    /// stopped answering. `'q'` with rate 0 was never probed; the simulator
    /// assumes the worst.
    pub zero_rate_hangs: bool,
    /// Closing the last TCP connection stops a playing stream (true). When
    /// false the DAC plays out its buffer and underflows.
    ///
    /// ED2 r331 showed Idle with no underflow flag on the next hello. j4cDAC
    /// only frees the connection state on close and never stops playback.
    pub close_stops_playback: bool,
    /// Light-engine flags mirror the e-stop state (`0x3` while stopped) rather
    /// than `0x1`.
    pub light_engine_flags_mirror_state: bool,
}

impl FirmwareProfile {
    /// Ether Dream 1 running the open-source j4cDAC firmware.
    ///
    /// Facts come from `firmware/net/point-stream.c` and `firmware/dac.c`
    /// of the j4cDAC repository, not from hardware.
    pub fn ed1_j4cdac() -> Self {
        Self {
            name: "ed1-j4cdac",
            provenance: Provenance::FirmwareSource,
            notes: "From j4cDAC source. DAC_BUFFER_POINTS=1800, broadcast advertises 1799. \
                    The 'v' build string is set at compile time, so the simulator reports a \
                    synthetic one. hw_revision is the board revision and assumed 0.",
            mac_address: [0x02, 0xed, 0x00, 0x00, 0x00, 0x01],
            hw_revision: 0,
            sw_revision: 2,
            buffer_capacity: 1799,
            max_point_rate: 100_000,
            version_string: Some("sim-ed1-j4cdac"),
            always_set_playback_flags: 0,
            ring_points: 1799,
            naks_full: false,
            begin_while_idle_acked: true,
            stop_while_idle_naks: true,
            stale_fullness_until_prepare: true,
            estop_clear_immediate: true,
            unknown_command_resets_tcp: true,
            data_partial_write_then_nak_invalid: true,
            data_while_idle_nak_invalid: true,
            enforce_single_client: false,
            rate_nak_consumes_one_byte: true,
            zero_rate_hangs: true,
            close_stops_playback: false,
            light_engine_flags_mirror_state: true,
        }
    }

    /// Ether Dream 2 running build `r331-ed4bef5`, probed on real hardware.
    pub fn ed2_r331() -> Self {
        Self {
            name: "ed2-r331",
            provenance: Provenance::HardwareVerified,
            notes: "Probed over TCP, with the UDP broadcast recorded once (hw_revision 10, \
                    sw_revision 2, capacity 3899, max rate 100000). The ring holds exactly \
                    the advertised 3899 points: a full ring answers a further write with \
                    NAK-Invalid and fullness unchanged, never NAK-Full. The 'v' reply is 32 \
                    raw bytes, the NUL-padded build string with no response or command \
                    prefix. A 'b', 'u' or 'q' with a rate above max is NAKed after consuming \
                    only the opcode, so its argument bytes are parsed as further commands \
                    (so a low-water mark of 0 would become two e-stops). A rate of 0 hung \
                    the firmware until power-cycled. The upper byte of the playback flags \
                    is not constant: 0x5f before one power cycle and 0x77 after it (0x7731 \
                    idle). All TCP clients share one playback state. Probed at \
                    169.254.102.149; after the power cycle the DAC was at 192.168.254.66.",
            mac_address: [0x02, 0xed, 0x00, 0x00, 0x00, 0x02],
            hw_revision: 10,
            sw_revision: 2,
            buffer_capacity: 3899,
            max_point_rate: 100_000,
            version_string: Some("r331-ed4bef5"),
            always_set_playback_flags: 0x5f31,
            ring_points: 3899,
            naks_full: false,
            begin_while_idle_acked: true,
            stop_while_idle_naks: true,
            stale_fullness_until_prepare: true,
            estop_clear_immediate: true,
            unknown_command_resets_tcp: true,
            data_partial_write_then_nak_invalid: true,
            data_while_idle_nak_invalid: true,
            enforce_single_client: false,
            rate_nak_consumes_one_byte: true,
            zero_rate_hangs: true,
            close_stops_playback: true,
            light_engine_flags_mirror_state: true,
        }
    }

    /// Ether Dream 3. **Unverified**: a copy of [`Self::ed2_r331`] with a
    /// guessed larger buffer.
    pub fn ed3() -> Self {
        Self {
            name: "ed3",
            provenance: Provenance::Unverified,
            notes: "Copy of ED2 r331 with a guessed 4095-point buffer. No hardware probed.",
            mac_address: [0x02, 0xed, 0x00, 0x00, 0x00, 0x03],
            buffer_capacity: 4095,
            ring_points: 4095,
            version_string: Some("sim-ed3"),
            ..Self::ed2_r331()
        }
    }

    /// Ether Dream 4. **Unverified**: a copy of [`Self::ed2_r331`] with a
    /// guessed larger buffer.
    pub fn ed4() -> Self {
        Self {
            name: "ed4",
            provenance: Provenance::Unverified,
            notes: "Copy of ED2 r331 with a guessed 8191-point buffer. No hardware probed.",
            mac_address: [0x02, 0xed, 0x00, 0x00, 0x00, 0x04],
            buffer_capacity: 8191,
            ring_points: 8191,
            version_string: Some("sim-ed4"),
            ..Self::ed2_r331()
        }
    }

    /// Every preset, oldest firmware first.
    pub fn all() -> Vec<Self> {
        vec![
            Self::ed1_j4cdac(),
            Self::ed2_r331(),
            Self::ed3(),
            Self::ed4(),
        ]
    }

    /// Find the preset whose `'v'` build string equals `version` exactly.
    ///
    /// Intended for log messages only. Synthetic simulator strings also match.
    pub fn identify(version: &str) -> Option<Self> {
        Self::all()
            .into_iter()
            .find(|p| p.version_string == Some(version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_unique_names_macs_and_versions() {
        let all = FirmwareProfile::all();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.name, b.name);
                assert_ne!(a.mac_address, b.mac_address);
                assert_ne!(a.version_string, b.version_string);
            }
        }
    }

    #[test]
    fn ring_holds_at_least_advertised_capacity() {
        for p in FirmwareProfile::all() {
            assert!(p.ring_points >= p.buffer_capacity, "{}", p.name);
        }
    }

    #[test]
    fn identify_matches_exact_build_string() {
        assert_eq!(
            FirmwareProfile::identify("r331-ed4bef5").map(|p| p.name),
            Some("ed2-r331")
        );
        assert!(FirmwareProfile::identify("r331").is_none());
        assert!(FirmwareProfile::identify("").is_none());
    }

    #[test]
    fn unverified_presets_are_marked() {
        assert_eq!(FirmwareProfile::ed3().provenance, Provenance::Unverified);
        assert_eq!(FirmwareProfile::ed4().provenance, Provenance::Unverified);
        assert_eq!(
            FirmwareProfile::ed2_r331().provenance,
            Provenance::HardwareVerified
        );
    }
}
