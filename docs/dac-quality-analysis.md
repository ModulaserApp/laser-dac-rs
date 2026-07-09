# DAC Output Quality & Reliability Analysis

Deep review of all DAC paths (excluding `lasercube_usb`), July 2026. Six parallel
reviews: shared scheduling core, Helios, Ether Dream, IDN, LaserCube WiFi, AVB
(+ oscilloscope). Protocol-level claims for Ether Dream were cross-checked
against the j4cDAC firmware source; Helios against the official C++ SDK.

---

## Cross-cutting themes

Five patterns recur across backends. Fixing them once at the seam fixes several
DACs at a time.

**1. Buffer-estimate arithmetic quietly erodes the underrun margin.**
The target buffer exists to absorb jitter, but three independent bugs eat it:

- Ether Dream double-counts every chunk: the data ACK's `buffer_fullness`
  already includes the points just sent (verified against j4cDAC firmware), yet
  the backend calls `record_status(fullness)` *and* `record_send(count)`
  (`ether_dream/backend.rs:224-227`). Steady-state real fullness sits one full
  chunk *below* target.
- `SoftwareDecayEstimator::record_send` rebases from a floor-truncated read,
  baking in ~0.5 points of upward bias per rebase (`software_decay.rs:33-45`).
  At ~1000 writes/sec this is ~500 phantom points/sec — the estimator believes
  the buffer is full while the device runs dry.
- `RuntimeAuthorityEstimator` ignores its `pps` argument and returns raw queue
  length in *device-sample-rate samples*, while the scheduler compares it
  against `target_secs × pps` points (`runtime_authority.rs:47-49`). For AVB at
  30 kpps into 96 kHz, a "20 ms" target is really a **6.25 ms** audio cushion —
  less than one CoreAudio callback quantum. Chronic underrun flicker. Same bug
  in the oscilloscope backend.

**2. Sleep/pacing loops assume `thread::sleep` is accurate.**
`sleep_with_control_check` decrements `remaining` by the *nominal* slice, not
elapsed time (`output_model/mod.rs:49-65`; same in `reconnect.rs:83-104`). On
Windows' default 15.6 ms timer a nominal 20 ms sleep can take >150 ms. The
LaserCube transport worker caps throughput at one packet per bare
`thread::sleep(≤1ms)` wake (`transport/worker.rs:98`) — ceiling ≈9 kpps on a
default Windows timer, guaranteed starvation at 30 kpps. And the UdpTimed
adapter's `next_send = now` clamp (`udp_timed.rs:51-55`) discards lost time
with no bounded catch-up, so every stall permanently drains the device buffer.
The crate already has the right tool (`sleep_until_precise` hybrid busy-wait) —
it's just not used everywhere, deadlines aren't absolute everywhere, and no
scheduler thread requests elevated priority (`session.rs:252`).

**3. Fire-and-forget UDP with no liveness or state reconciliation.**
IDN sends channel config exactly once per session — one lost datagram at start
means permanently black output with `is_connected() == true`
(`idn/dac/stream.rs:193-199`; `last_config_time` is written but never read).
IDN also never requests ACKs (`write_frame_with_ack` and `ping` are dead code
in the backend path), so a powered-off projector is never detected and
reconnect never fires. LaserCube never reconciles its intended
`output_enabled` against the device-reported flag (`transport/worker.rs:461-474`)
— lose both duplicate 0x80 enables in one WiFi burst and the show stays dark
while telemetry looks healthy.

**4. `stable_id` contract violations break reconnect-by-id.**
CONTEXT.md promises ids stable across IP/port changes. In practice:
LaserCube WiFi uses the IP (`discovery.rs:399-401`) — DHCP lease change breaks
auto-reconnect even though the serial is already parsed and stored; Helios uses
bus/port-path, not serial — replug into another port (or swap two units' ports)
and the session reconnects to the *wrong projector*; IDN uses hostname —
factory-default hostnames on identical fixtures collide (unit_id is available);
AVB uses name + enumeration index — unplugging unit #0 silently rebinds a
session to unit #1.

**5. Per-chunk stateless resampling (AVB, oscilloscope).**
Each chunk is resampled endpoint-inclusive with no fractional phase carried
across calls (`avb/backend.rs:766-801`, `oscilloscope/backend.rs:467-491`).
The inter-chunk source interval is always traversed in exactly one output
sample → ~1.6× galvo-velocity transient at every chunk boundary (a 50–100 Hz
banding artifact), plus C1 breaks from clamped spline context. Fix once with a
shared stateful resampler (carry phase + last 3 samples; reset on
connect/stop/reconnect).

---

## Prioritized fix list

Ordered by (impact on real shows) × (confidence) ÷ (effort).

| # | Fix | Files | Effort |
|---|-----|-------|--------|
| 1 | **Ether Dream: drop the `record_send` after `record_status`** (ACK fullness already includes the send) | `ether_dream/backend.rs:227` | one line |
| 2 | **UdpTimed: recompute `chunk_duration` unconditionally** — guard only watches chunk size, so a pps change in the clamped regime (IDN ≥17.9 kpps) keeps the old send period → feed at half rate, continuous underrun | `output_model/udp_timed.rs:36-43` | one line |
| 3 | **IDN: resend channel config every ~200 ms** (spec expectation; removes the permanent-black-on-one-lost-packet failure) | `idn/dac/stream.rs:193-199` | small |
| 4 | **AVB/oscilloscope: convert queue depth to pps-points** in `RuntimeAuthorityEstimator` (report `queue.len() × pps / sample_rate`) | `buffer_estimate/runtime_authority.rs`, `avb/backend.rs:119-127`, `oscilloscope/backend.rs:48-59` | small |
| 5 | **Ether Dream: never silently truncate** — return `WouldBlock` when `available < points.len()` instead of clamping and reporting `Written` (adapter commits the full slice; the tail is dropped forever → mid-shape tearing at high pps) | `ether_dream/backend.rs:157-159` | small |
| 6 | **SoftwareDecay/StatusDecay: carry fractional consumption on rebase** (advance anchor by `consumed/pps` instead of `now`) | `buffer_estimate/software_decay.rs:33-45`, `status_decay.rs:39-53` | small |
| 7 | **Ether Dream: NAKs are backpressure, not disconnects** — map `Nak::Full` and data-NAK-while-Prepared/Playing to `WouldBlock`; decay fullness only when `playback == Playing`; clamp rate to `max_point_rate` | `ether_dream/backend.rs:184-204`, `network_fifo.rs:96-101` | medium |
| 8 | **IDN: request ACKs periodically for liveness** (interleave `CNLMSG_ACKREQ` every ~250-1000 ms, N misses → disconnected so reconnect engages; also validates `connect()`) | `idn/backend.rs:61-68`, `idn/dac/stream.rs` | medium |
| 9 | **LaserCube: send full packets, multiple per wake** — steady state currently degenerates to ~1 kHz × ~30-point packets (3-5× WiFi airtime, `wait_buffer_sleep` dead due to 1 ms clamp) and one-packet-per-wake caps throughput below 30 kpps on Windows | `lasercube_network/transport/worker.rs:293-368,439-450` | medium |
| 10 | **Helios: leak handle only on true disconnect** — `Drop` unconditionally `mem::forget`s the USB handle, so a graceful stop leaks a claimed interface → next open in the same process fails `LIBUSB_ERROR_BUSY` | `helios/backend.rs:198-202` | small |
| 11 | **Stable ids from hardware identity**: LaserCube→serial, Helios→device name, IDN→unit_id, AVB→platform UID where available | per-protocol discovery | medium |
| 12 | **Deadline-based sleeps + bounded catch-up + thread priority**: fix `sleep_with_control_check` to an absolute deadline; allow UdpTimed/LaserCube ≤2-3 chunks of catch-up after a stall; name + raise priority of scheduler threads | `output_model/mod.rs:49-65`, `udp_timed.rs:51-55`, `session.rs:252` | medium |
| 13 | **Frame-capacity clamp for authored frames** — >4095-point frames reach the Helios encoder unclamped (SDK refuses these; firmware behavior undefined) | `engine.rs:291-300` or `helios/backend.rs` | small |
| 14 | **Shared stateful resampler** for AVB + oscilloscope (phase carried across chunks) | `resample.rs`, both backends | medium |
| 15 | **AVB: detect dead cpal stream** — error callback only logs; set a `stream_failed` flag checked by `try_write_points`/`is_connected` so reconnect engages | `avb/backend.rs:209-211,384-386` | small |

---

## Per-DAC findings

### Shared core (`src/presentation/`, `src/buffer_estimate/`, `src/reconnect.rs`)

**High**
- `UdpTimedAdapter::recompute_chunk` skips `chunk_duration` recompute when the
  chunk stays clamped but pps changed (`udp_timed.rs:36-43`) — fix #2. Certain.
- `SoftwareDecayEstimator` rebase truncation bias (`software_decay.rs:33-45`) —
  fix #6. Primary victim: any SoftwareDecay-paced backend with unconditional
  accept, where the estimator is the *only* pacing.
- `NetworkFifoAdapter` has no minimum write quantum (`network_fifo.rs:56-61`):
  ~1000 tiny writes/sec in steady state — an amplifier for the rebase bias and
  pure overhead. Gate writes on a minimum deficit (e.g. 5 ms of points).

**Medium**
- `sleep_with_control_check` nominal-decrement oversleep (`output_model/mod.rs:49-65`) — fix #12.
- UdpTimed `next_send = now` clamp discards backlog with no catch-up (`udp_timed.rs:51-55`).
- `WouldBlock` spin in NetworkFifo never processes control messages — **a
  Disarm (shutter close) is delayed indefinitely while spinning**
  (`network_fifo.rs:72-103`). Safety-relevant; also add a staleness timeout →
  treat as stalled/disconnected.
- Scheduler threads are unnamed, default priority (`session.rs:252`).
- Authored frames > `frame_capacity` never clamped (`engine.rs:291-300`) — fix #13.
- Oscilloscope: underrun/mute snaps beam to center instead of holding last
  position (`oscilloscope/backend.rs:169-206`); per-chunk resampler phase reset.

**Low**
- `discard_cached` documented as a driver obligation but has zero call sites
  (`content_source.rs:44-49`) — wire it or fix the contract docs.
- Reconnect sleeps a full backoff *before* attempt #1 (`reconnect.rs:139`) —
  instantly-recoverable disconnects blank output for a whole backoff period.
- Armed-with-no-frame blank fill ignores `IdlePolicy::Park` (`engine.rs:124-127`).
- Color delay on cyclic frame-swap frames uses previous-frame carry rather than
  rotation — wrong when hardware replays a frame (source stall).
- `DualTrackAckEstimator::LATENCY_POINT_ADJUSTMENT` is a point count, not a
  duration — margin scales inversely with pps (`dual_track_ack.rs:15`).

### Helios (`src/protocols/helios/`)

**High**
- Unconditional handle leak in `Drop` (`backend.rs:198-202`) — fix #10. Gate
  the macOS anti-segfault leak on an actual fatal-disconnect flag; have the
  driver call `disconnect()` on graceful exit.
- No >4095-point clamp anywhere in the write path (`native.rs:390-417`) — fix #13.

**Medium**
- `assert_eq!` on interrupt-write length can panic the output thread
  (`native.rs:334-336`) — return an error instead.
- Partial bulk transfers ignored (`native.rs:244,258-261`) — a truncated frame
  is reported as `Written` and displayed corrupted. One-line length check.
- Runtime `set_pps` unvalidated; Helios encoder wraps pps to u16 garbage
  (`native.rs:412-413`). Clamp to caps in the backend.
- One transient bulk-write error (e.g. a single `Timeout`) → full disconnect +
  ≥150 ms dark reconnect (`usb_frame_swap.rs:70-75`), while status polls
  tolerate 50 consecutive failures. Retry the cached frame a few times first.
- `stable_id` is port-path, not serial (`discovery.rs:29-31`) — contradicts
  CONTEXT.md; wrong-projector risk in multi-DAC rigs. Device name via
  `CONTROL_GET_NAME` is implemented but unused.

**Low**
- `connect()` fallback re-enumeration hardcodes device index 0 (`backend.rs:40-49`).
- `status_error_count` never reset in `connect()` (`backend.rs:144-167`).
- Fixed 1 ms status polling for the entire frame duration — sleep ~half the
  known frame time first (`usb_frame_swap.rs:25-28`).
- `color_to_u8` truncates instead of rounds (`point.rs:95-97`) — sub-LSB dark bias.
- `Pipe` (STALL) treated as physical disconnect; try `clear_halt` once first.
- Firmware version probed then discarded (`native.rs:134-137`).

*Done well:* wire format + 64-byte transfer workaround + init sequence verified
exact against the official SDK; poll-before-write contract honored with a
redundant-poll optimization; good error taxonomy with escalation thresholds;
zero per-frame allocation; tear-free single-transfer frames.

### Ether Dream (`src/protocols/ether_dream/`)

**High**
- Estimator double-count (`backend.rs:218-227`) — fix #1. Verified against
  j4cDAC firmware: ACK status is sampled *after* the ring write.
- Silent truncation to `available` reported as `Written` (`backend.rs:157-159`)
  — fix #5. Continuous mid-shape tearing whenever target > headroom (always at
  pps ≥ ~90k with a 20 ms target vs the 1799-point ring).
- Warmup/Cooldown livelock: `WouldBlock` returned without ever refreshing
  status, and nothing else sends a command (`backend.rs:126-128`). Reachable
  via the estop→warmup transition. Send a rate-limited `ping` in that branch.
- Every NAK is fatal (`backend.rs:184-204`) — fix #7. Firmware answers buffer
  overflow with NAK-`I` on a healthy connection; current code disconnects and
  restarts the stream over a flow-control event.

**Medium**
- Clear-e-stop diagnosis unreachable: firmware answers `'!'` NAK, which errors
  out of `submit()` before the intended interlock check (`backend.rs:101-125`);
  result is a tight connect/clear/NAK/reconnect loop. Also: auto-clearing
  e-stop on every write deserves an opt-in flag.
- `MIN_POINTS_BEFORE_BEGIN = 256` ignores pps/target (`backend.rs:153`) — up to
  256 ms extra start latency at 1 kpps; only 2.6 ms of cover at 100 kpps.
- Estimators decay while playback is Idle/Prepared (buffer is static then) —
  over-admission → overflow NAKs (`backend.rs:242-255`, `status_decay.rs:45-54`).
- 500 ms read timeout escalates straight to disconnect; no write timeout at all
  (`dac/stream.rs:381,405-417`) — a Wi-Fi latency spike restarts the stream; a
  hung DAC can wedge the session thread in `write_all` forever.
- Idle-retry path triggers on *any* error kind including desynced IO errors
  (`backend.rs:189-200`) — retrying into a desynced stream makes the firmware
  close the connection (unknown-byte handler).
- Discovery binds UDP 7654 without `SO_REUSEADDR`, reads max 3 datagrams,
  silently returns empty on bind failure (`mod.rs:71-78`, `discovery.rs:56-89`)
  — any other ED software on the host makes the DAC invisible to reconnect.

**Low**
- `stop()` errors when already idle; `Nak::Full` unmapped (dead but dangerous
  for emulators); stale "not yet consulted" estimator comment; both axes
  inverted = 180° vs stock ED tooling (document or make configurable);
  reachable `assert!` panic in `Stream` data path; firmware's sample-accurate
  rate-change queue (`'q'` + CHANGE_RATE bit) unused.

*Done well:* wire format verified exact; status harvested from every response
including NAKs (that's what makes the underflow retry work); underflow race
handled per actual firmware semantics; ring headroom math matches firmware
(`capacity − fullness − 1`); symmetric ±32767 quantization with full 16-bit
color; reconnect swaps the whole backend object.

### IDN (`src/protocols/idn/`)

**High**
- Config sent once, never refreshed (`dac/stream.rs:193-199`) — fix #3.
- No liveness/ACKs in the streaming path (`backend.rs:61-68`) — fix #8. Dead
  device streams into the void forever; `connect()` succeeds unconditionally.
- Frame-mode fragmentation wrong (per-fragment durations, advancing timestamps,
  no `LSTFRG` marker; `dac/stream.rs:329-407`). Latent — backend hardcodes Wave
  mode — but `set_frame_mode(Frame)` is public API.

**Medium**
- Timestamp drift: floor-truncated per-chunk durations (~112 µs/s at 30 kpps)
  plus `next_send = now` stall discard → chunks arrive ever later than their
  timestamps → chronic receiver underrun after ~90 s. Fix with an exact
  `total_points × 10⁶ / pps` accumulator (`dac/stream.rs:347,399,519`).
- No re-anchor after idle gaps: resuming after keepalive-only intervals sends
  timestamps arbitrarily far in the past (`dac/stream.rs:476-478`).
- No latency lead: first chunk timestamped "now", everything just-in-time —
  receiver jitter tolerance is whatever the *receiver* adds. Send 2-4 chunks
  up front (pairs with the timestamp refactor).
- Discovery single-shot (one scan burst, one servicemap request, no retries;
  a late scan response consumed by the servicemap read drops the whole server)
  (`mod.rs:187,354-414`, `discovery.rs:77-78`).
- 16-bit color truncated to 8-bit: backend hardcodes `PointXyrgbi` while
  hi-res formats are fully implemented and descriptor-correct (`backend.rs:168`).
  IDN is the one family where the crate's 16-bit model could ship end-to-end.
- `stable_id` = hostname; duplicate factory hostnames collide (unit_id exists)
  (`discovery.rs:70-72`). Also: only the *first* laser service per server is
  exposed — multi-output IDN servers surface as one device (`discovery.rs:78`).

**Low**
- Any stale datagram fails `recv_response` (loop-and-discard instead);
  channel-id derivation aliases service ids > 64; estimator counts enqueue-time
  and is never reset; false capacity-reuse comment (`mem::take` allocates every
  write); duplicate conflicting 0xEB error-code definitions; misleading
  `InvalidPointFormat` for scan_speed 0; stale "IDN-shaped: 4096" test comment
  (cap is 179); double close on shutdown.

*Done well:* wire layouts spec-exact with roundtrip tests; correct u32
timestamp wrap and sequence numbering; even fragment distribution (no runt
chunks); keepalive as void channel message; graceful channel+session close;
conservative 1454-byte MTU; thorough multi-interface discovery.

### LaserCube WiFi (`src/protocols/lasercube_network/`)

**High**
- Steady-state pacing degenerates to ~1 kHz × ~30-point packets instead of
  ~215 Hz × 140-point packets (`transport/worker.rs:293-368`, `pacing.rs:14-22`)
  — fix #9. 3-5× WiFi airtime overhead exactly where loss causes flicker;
  the per-profile `wait_buffer_sleep` (4-6 ms) is dead code (always clamped to
  the 1 ms `MAX_IDLE_SLEEP`).
- One packet per wake + bare `thread::sleep` → ~9 kpps ceiling on default
  Windows timer resolution; no multi-packet catch-up after oversleep
  (`transport/worker.rs:98`).
- `stable_id` is the IP (`discovery.rs:399-401`) — DHCP change breaks
  auto-reconnect; serial number is already parsed (`status.rs:135`).

**Medium**
- `intensity` silently dropped (`protocol.rs:61-71`) — intensity-as-dimmer
  content renders full-bright (all other backends honor it). Premultiply into
  RGB, matching the convention in `receiver/parser.rs:449`.
- ACK handling overwrites `free_estimate` with the last ACK, ignoring in-flight
  packets and reordering; the purpose-built `DualTrackAckEstimator` (which
  solves exactly this) is dead code despite `buffer_estimate/mod.rs:15-16`
  claiming it's used (`transport/worker.rs:249-274`).
- Stale-comms timeout (4 s) < 2× active poll period (2×2.5 s): one lost poll
  reply while armed-but-idle → spurious full disconnect (`transport.rs:40-41`).
- `connect()` never verifies reachability; reports connected/usable for 4 s
  while dropping points; re-`connect()` keeps a stale transport
  (`handle.rs:25-65`, `backend.rs:48-55`).
- Output-enable intent never reconciled against device-reported state —
  correlated loss of both 0x80 datagrams leaves the show dark with healthy
  telemetry (`transport/worker.rs:461-474`).
- `buffer_max.max(6000)` inflates `buffer_total` for devices reporting less →
  pacer refuses to send, burst/starve cycling (`transport/state.rs:69-72`).
- `threshold_supported` version check is wrong (`firmware_minor > 23` passes
  regardless of major) (`command.rs:42-44`).

**Low**
- CONTEXT.md says LaserCube WiFi is UdpTimed; code says NetworkFifo (doc drift,
  also bit the AVB entry). Strict `len == 64` full-info parse rejects future
  firmware. `try_write_points` clones the point buffer per write (use
  `mem::take`). No ring-buffer clear on connect/re-arm (0x8D is test-only) —
  comment the intent. `set_output(true)`/`stop_output` generation race on
  concurrent handle use. Only final ACK per drain batch updates RTT diagnostics.

*Done well:* discovery is exemplary (per-interface directed + limited
broadcasts, passive listener, source-IP-authoritative, VPN/AP-mode correct);
safety-first output handling (priority command channel, ×2 command repeats,
output-off on every teardown path); generation gating drops stale in-flight
points on disarm; RAII queue reservation; well-tested pure cores with a golden
wire test.

### AVB (`src/protocols/avb/`)

**High**
- Estimator unit mismatch (audio samples vs pps-points) — fix #4. With the
  session's hardcoded 20 ms target, the real cushion at 30 kpps/96 kHz is
  6.25 ms → chronic underrun flicker (underrun holds XY, blanks RGB).
- Dead cpal stream never detected (`backend.rs:209-211,384-386`) — fix #15.
  Output freezes silently; reconnect never engages.
- `connect()` init-timeout branch joins the worker while `stop_tx` is still
  alive → deadlock if `open_stream` is slow-but-successful (`backend.rs:353-357`).
  Drop `stop_tx` before `join()`.

**Medium**
- Per-chunk stateless resampling (chunk-boundary velocity transients at ~50 Hz)
  (`backend.rs:766-801`) — fix #14.
- f32 sample format hardcoded — I16/I32-only devices (ALSA, many ASIO) pass
  discovery then fail every connect (`backend.rs:202-214,543-554`).
- Hardcoded `BufferSize::Fixed(256)` on Windows, no fallback — fails on ASIO
  drivers with a different configured size (`backend.rs:682-694`).
- Presentation sessions hardcode 20 ms target, bypassing the 50 ms network
  default used by the stream path (`session.rs:407-413` vs `stream/mod.rs:1539-1552`).
- Discovery over-matches: any ≥5-channel output (7.1 onboard, HDMI) is offered
  as a laser DAC; blacklist has one entry (`backend.rs:588-650`).
- `duplicate_index` shifts when the device set changes → reconnect can bind a
  session to a different physical projector (`backend.rs:611-633`).
- Intensity dropped on 5-channel devices instead of premultiplying RGB
  (`backend.rs:851-858`).
- Caps never updated with the detected sample rate — pps above device rate
  silently decimates with no signal to the user (`mod.rs:22-29`).

**Low**
- CONTEXT.md lists AVB as UdpTimed (it's NetworkFifo — correctly so).
- `scan()` swallows discovery errors; `connect()` enumerates the device list
  twice (TOCTOU); DC-coupling requirement undocumented.

*Done well:* closed-loop pacing off real queue depth (no wall-clock vs audio
clock drift by construction); genuinely real-time-safe callback (lock-free pop,
zero alloc/log/syscall); laser-safe underrun semantics (hold XY, blank RGB);
correct handling of cpal's `!Send` stream via an owner thread; strong tests.

---

## Test-coverage gaps (quality-critical, currently unpinned)

1. **No pacing/timing tests at all** — nothing exercises `recompute_chunk`
   across a pps change in the clamped regime (would have caught fix #2),
   deadline slip/catch-up, or sleep accounting.
2. **No multi-rebase estimator drift test** — a 1000-rebase accumulation test
   would have caught the truncation bias.
3. **Zero oscilloscope backend tests** (resampler continuity, underrun, mute).
4. **No authored-frame > `frame_capacity` test** (only transition trimming is
   covered).
5. **No bound on NetworkFifo write cadence / minimum chunk size.**
6. **Disarm latency during the `WouldBlock` spin is untested** (safety).
7. `drain_via_estimator` untested from the presentation side.

---

## Suggested sequencing

- **Wave 1 (small, certain, high impact):** fixes 1, 2, 4, 5, 6, 10, 13, 15 +
  the AVB connect deadlock. Each is a few lines; together they remove the main
  systematic underrun/tearing sources on Ether Dream, IDN, AVB, oscilloscope,
  and the Helios restart-in-process failure.
- **Wave 2 (protocol robustness):** IDN config resend + ACK liveness (3, 8),
  Ether Dream NAK-as-backpressure + playing-aware decay (7), LaserCube packet
  coalescing + multi-send (9), LaserCube output-enable reconciliation.
- **Wave 3 (identity & pacing infrastructure):** stable-id overhaul (11),
  deadline-based sleeps + catch-up + thread priority (12), shared stateful
  resampler (14), timestamp accumulator for IDN.
- **Alongside:** update CONTEXT.md (AVB/LaserCube output-model entries, Helios
  stable_id claim) and add the missing test classes above — the pacing and
  estimator-drift tests in particular would have caught three of the top-ten
  bugs before they shipped.
