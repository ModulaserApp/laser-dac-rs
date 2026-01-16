# Streaming Refactor Horizon (Target API + Architecture)

This document is the “point on the horizon” for the streaming refactor: where we want the crate to end up after the redesign, and why. It is intentionally idealistic and is allowed to break existing public APIs/patterns.

It is also intentionally conservative about refactoring: we should keep existing, already-good primitives (e.g. `LaserPoint` in `src/types.rs`) unless a change is required to enable a streaming-first architecture.

## Why (laser hardware reality)

Laser DACs output a continuous point signal at a configured points-per-second (PPS). Many devices are FIFO-like (“top up” buffers); others are frame-quantized (e.g. Helios double-buffering). Applications should not need device-specific timing hacks to produce stable output.

The core problem we want to solve is: **uniform pacing + backpressure + timebase** across all DACs.

## Why this is better than the current approach

This refactor is not “architecture for architecture’s sake”. It targets failure modes that show up immediately in practice, especially for audio-reactive visuals.

- **Bounded latency becomes a first-class contract**
  - For oscilloscope/vectorscope style output, “too much buffering” is as bad as underrun: the visuals stop feeling real-time.
  - `target_queue_points` + `scheduled_ahead_points` provides a portable way to keep output latency around a target range across DACs.
  - This prevents the common failure mode where a backend is effectively “always writable” (e.g. OS accepts UDP sends immediately), the producer runs far ahead, and latency drifts into seconds.

- **Uniform pacing across DACs (no device-specific hacks in the app)**
  - Today, an app effectively needs to know which DACs provide meaningful backpressure and which don’t.
  - With a stream scheduler, pacing happens even when the backend cannot report “busy”, so behavior stays consistent across Ether Dream, Helios, IDN, LaserCube, etc.

- **A proper timebase makes audio correctness measurable**
  - Direct audio projection (XY scope / “frequency projecting”) depends on mapping audio time to beam time.
  - `StreamInstant` is an exact point clock: producers can align audio windows to `ChunkRequest.start` and debug using `start` + `scheduled_ahead_points` as observable signals.

- **Underrun behavior becomes predictable (and safer)**
  - Underruns in laser output can freeze on a bright point or create harsh discontinuities.
  - A library-owned underrun policy (blank/park/repeat/stop) makes worst-case behavior consistent across devices.

## Design goals

- Streaming-first: the primary output abstraction is a **stream of point chunks** at a fixed PPS.
- Explicit integer timebase: `StreamInstant(u64)` meaning “points since stream start”.
- Uniform backpressure model: “accept now” vs “would block” is consistent across backends.
- Centralized device capabilities: chunk sizing and validation are based on `Caps`, not tribal knowledge.
- Library-owned underrun behavior: repeat/blank/park/stop is consistent across DACs.
- Make stream behavior observable (queue depth + basic stats) so timing issues are debuggable in real rigs.
- Frames remain supported, but as **adapters** that feed a stream.
- Supports audio-reactive visuals with predictable latency by providing a stream timebase and bounded queue depth.
- Enables direct audio projection by making beam-time alignment explicit via `ChunkRequest.start` + PPS.
- Do not refactor types unnecessarily: keep `LaserPoint` (`src/types.rs`) and its `u16` color/intensity representation.

## What we keep vs what changes

### Keep (unless proven necessary to change)

- `LaserPoint` as the canonical point type (normalized `x/y: f32`, `r/g/b/intensity: u16`).
- `DacType` for describing supported hardware families.
- Device-specific protocol modules continue to own their on-wire formats.

### Change (core surface)

- Replace “frames + workers” as the core API with “devices + streams”.
- Frames become adapter-level convenience (`FrameAdapter`, `FrameSource`), not the core output type.

## Proposed public API (target)

### Discovery / opening

Discovery yields metadata including capabilities. Opening yields a connected `Device` that can start a streaming session.

```rust
pub struct DeviceInfo {
    /// Stable, unique identifier used for (re)selecting devices.
    ///
    /// This should be unique across same-name devices and as stable as possible across restarts.
    /// Examples: USB path + serial, device serial, MAC address, IP:port.
    pub id: String,
    pub name: String,
    pub kind: DacType,
    pub caps: Caps,
}

pub fn list_devices() -> Result<Vec<DeviceInfo>, Error>;
pub fn open_device(id: &str) -> Result<Device, Error>;
```

### Capabilities

Capabilities let the stream scheduler choose safe chunk sizes and behaviors.

```rust
#[derive(Clone, Debug)]
pub struct Caps {
    pub pps_min: u32,
    pub pps_max: u32,

    /// Maximum number of points allowed per chunk submission.
    /// (Helios example: ~4095 points per frame.)
    pub max_points_per_chunk: usize,

    /// Some DACs dislike per-chunk PPS changes.
    pub prefers_constant_pps: bool,

    /// Best-effort: can we estimate device queue depth/latency?
    pub can_estimate_queue: bool,

    /// The scheduler-relevant output model (not the physical transport).
    pub output_model: OutputModel,
}

#[derive(Clone, Debug)]
pub enum OutputModel {
    /// Frame swap / limited queue depth (e.g. Helios-style).
    UsbFrameSwap,
    /// FIFO-ish buffer where “top up” is natural (e.g. Ether Dream-style).
    NetworkFifo,
    /// Timed UDP chunks / OS send may not reflect hardware pacing (e.g. some UDP protocols).
    UdpTimed,
}
```

### Stream timebase

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamInstant(pub u64); // points since stream start

impl StreamInstant {
    pub fn as_seconds(self, pps: u32) -> f64 {
        self.0 as f64 / pps as f64
    }
}
```

### Starting a stream

The device starts a stream with a fixed PPS and a target queue depth.

```rust
pub struct StreamConfig {
    pub pps: u32,

    /// Exact chunk size to request/write. If `None`, the library chooses a safe default
    /// based on `Caps` and the target queue.
    pub chunk_points: Option<usize>,

    /// Target amount of queued data expressed in points (not time).
    /// Typically 2–6 chunks worth.
    pub target_queue_points: usize,

    pub underrun: UnderrunPolicy,

    /// Whether to automatically open the hardware output gate when arming.
    /// When `false` (default), `arm()` only enables software output.
    /// When `true`, `arm()` will also open the hardware gate.
    /// Note: `disarm()` always closes the hardware gate for safety.
    pub open_output_gate_on_arm: bool,
}

pub enum UnderrunPolicy {
    RepeatLast,
    Blank,
    Park { x: f32, y: f32 },
    Stop,
}

impl Device {
    pub fn caps(&self) -> &Caps;
    pub fn info(&self) -> &DeviceInfo;
    pub fn id(&self) -> &str;
    pub fn name(&self) -> &str;
    pub fn kind(&self) -> &DacType;
    pub fn has_backend(&self) -> bool;
    pub fn is_connected(&self) -> bool;

    /// Starts a streaming session, consuming the device.
    ///
    /// The `Device` is consumed because each device can only have one active stream
    /// at a time. The backend is moved into the `Stream` to ensure exclusive access.
    /// This prevents accidental reuse of a device that's already streaming.
    ///
    /// Returns both the `Stream` and a copy of `DeviceInfo`, so you retain access
    /// to device metadata (id, name, capabilities) after starting.
    ///
    /// To recover the device for reuse, call `stream.into_device()`.
    pub fn start_stream(self, cfg: StreamConfig) -> Result<(Stream, DeviceInfo), Error>;
}
```

**Safety defaults (intent)**

- The default `UnderrunPolicy` should be safe (typically `Blank` or `Park`), not “repeat last bright point forever”.
- Make an explicit armed/disarmed output gate a first-class API so reconnects don’t accidentally lase without an explicit enable step.

**Out-of-band control (target direction)**

Professional software typically needs a control path that works even if the producer is stalled. Target direction:

```rust
/// Thread-safe control path for safety-critical actions.
#[derive(Clone)]
pub struct StreamControl { /* ... */ }

impl StreamControl {
    pub fn arm(&self) -> Result<(), Error>;
    pub fn disarm(&self) -> Result<(), Error>;
    pub fn is_armed(&self) -> bool;
    pub fn stop(&self) -> Result<(), Error>;
    pub fn is_stop_requested(&self) -> bool;
}

impl Stream {
    /// Returns a thread-safe control handle.
    pub fn control(&self) -> StreamControl;
}
```

### Error model (streaming)

Streaming needs an explicit “would block” outcome to enforce a uniform backpressure contract.

```rust
pub enum Error {
    /// The device/library cannot accept more data right now.
    ///
    /// Note: in the blocking API, `next_request()` should not normally surface this. It exists to
    /// enforce a uniform backend contract and to support potential future non-blocking APIs.
    WouldBlock,
    /// The stream was explicitly stopped via `StreamControl::stop()`.
    Stopped,
    /// The device disconnected or became unreachable.
    Disconnected(String),
    /// Invalid configuration or API misuse.
    InvalidConfig(String),
    /// Backend/protocol error (wrapped).
    Backend(Box<dyn std::error::Error + Send + Sync>),
}
```

### Stream operation (blocking mode)

In blocking mode, the library tells you exactly what to generate next and when.

```rust
pub struct ChunkRequest {
    pub start: StreamInstant,
    pub pps: u32,
    /// Number of points requested for this chunk.
    ///
    /// Semantics: this is fixed for the lifetime of the stream and equals `Stream::chunk_points()`.
    pub n_points: usize,

    /// How many points are currently scheduled ahead of `start`, according to the library.
    /// This is always well-defined (library-owned).
    ///
    /// This value represents the library's queue depth (in points) at the time of the request.
    pub scheduled_ahead_points: u64,

    /// Best-effort: points reported by the device/backend as queued for future output.
    /// Not all devices can report this reliably.
    pub device_queued_points: Option<u64>,
}

impl Stream {
    pub fn info(&self) -> &DeviceInfo;
    pub fn config(&self) -> &StreamConfig;
    pub fn control(&self) -> StreamControl;
    pub fn status(&self) -> Result<StreamStatus, Error>;

    /// Blocks until the stream wants the next chunk.
    pub fn next_request(&mut self) -> Result<ChunkRequest, Error>;

    /// Writes exactly `req.n_points` points for the given request.
    pub fn write(&mut self, req: &ChunkRequest, points: &[LaserPoint]) -> Result<(), Error>;

    pub fn stop(&mut self) -> Result<(), Error>;

    /// The resolved chunk size chosen for this stream (after applying caps/defaults).
    ///
    /// Semantics: fixed for the lifetime of the stream.
    pub fn chunk_points(&self) -> usize;

    /// Manually open the hardware output gate.
    pub fn open_output_gate(&mut self) -> Result<(), Error>;

    /// Manually close the hardware output gate.
    pub fn close_output_gate(&mut self) -> Result<(), Error>;

    /// Returns whether the hardware output gate is currently open.
    pub fn is_output_gate_open(&self) -> bool;

    /// Consume the stream and recover the device for reuse.
    ///
    /// This stops the stream, closes the output gate, and returns the
    /// underlying `Device` along with the final `StreamStats`.
    pub fn into_device(self) -> (Device, StreamStats);
}

pub struct StreamStatus {
    pub connected: bool,

    /// The resolved chunk size chosen for this stream.
    pub chunk_points: usize,

    /// Library-owned scheduled amount (always meaningful).
    pub scheduled_ahead_points: u64,

    /// Best-effort device/backend estimate.
    pub device_queued_points: Option<u64>,

    /// Optional high-level counters for diagnostics/UI.
    pub stats: Option<StreamStats>,
}

pub struct StreamStats {
    pub underrun_count: u64,
    pub late_chunk_count: u64,
    pub reconnect_count: u64,
    pub chunks_written: u64,
    pub points_written: u64,
}
```

**Blocking handshake semantics**

- `next_request()` returns the next required chunk request and blocks until the stream is ready
  to accept that chunk (or returns `Disconnected` / `Backend(...)` on failure).
- `write(req, points)` must provide exactly `req.n_points` points for that request (hard contract).
- If the caller is late (e.g. GC pause, render spike), the stream applies `UnderrunPolicy` to *fill the missed point interval* and advances internal time accordingly. This can cause a future `ChunkRequest.start` to jump forward relative to wall-clock progression.
- `ChunkRequest.scheduled_ahead_points` and `StreamStatus.scheduled_ahead_points` reflect the stream’s library-owned schedule after any underrun fill (i.e. “what will happen on the wire if you generate nothing further right now”).

**Fixed-count (budgeted) chunk model**

Treat each `ChunkRequest` as a fixed point-clock budget. Every stage in the producer pipeline must fit within that budget:

- “Extra points” such as blank travel, blank lead-in/out, pre/post settle dwell, and park/blank tails are not additions; they **consume** points from the same `req.n_points` budget.
- Safety stages that conceptually “insert points” (corner rounding, delta/accel limiting via subdivision) must be implemented in a budget-aware way:
  - either by spending budget (reducing content detail), or
  - by oversampling internally and then normalizing back to exactly `req.n_points`,
    followed by a **final length-preserving motion limiter** so normalization/decimation cannot reintroduce unsafe jumps.

### Stream operation (callback mode)

Callback mode is the “audio device” model: the library owns scheduling and you produce chunks on demand.

```rust
pub enum RunExit {
    /// A stop request was issued via out-of-band control.
    Stopped,
    /// The producer returned `None` (graceful completion).
    ProducerEnded,
    /// The device disconnected or became unreachable.
    Disconnected,
}

impl Stream {
    pub fn run<F, E>(self, producer: F, on_error: E) -> Result<RunExit, Error>
    where
        F: FnMut(ChunkRequest) -> Option<Vec<LaserPoint>> + Send + 'static,
        E: FnMut(Error) + Send + 'static;
}
```

**Allocation note (target direction)**

For high PPS, per-chunk `Vec` allocation can become visible. Target direction:

- Support a no-allocation producer shape where the producer fills a provided buffer
  (e.g. `FnMut(ChunkRequest, &mut [LaserPoint]) -> ProduceResult`), while keeping the `Vec`-based API as a convenience.

## Why `ChunkRequest` matters (audio-reactive visuals)

The request model enables audio-like correctness and makes stream behavior observable:

- **XY oscilloscope / vectorscope**: use `ChunkRequest.start` (and PPS) to select the correct audio window from a ring buffer and generate exactly `n_points`.
- **FFT / envelope visuals**: compute features over the chunk’s time window and render them deterministically into points.
- **Stability under load**: `scheduled_ahead_points` exposes whether you’re running ahead (growing latency) or falling behind (risking underrun), enabling graceful degradation strategies.

### Frames (adapter layer)

Frames remain supported as a convenience. They are not written to DACs directly; instead they feed a stream as a `ChunkRequest -> Vec<LaserPoint>` producer.

```rust
/// A point buffer to be cycled by the adapter.
pub struct Frame {
    pub points: Vec<LaserPoint>,
}

impl Frame {
    pub fn new(points: Vec<LaserPoint>) -> Self;
    pub fn empty() -> Self;
}

/// Converts a point buffer (frame) into a continuous stream.
///
/// The adapter cycles through the frame's points, producing exactly
/// `req.n_points` on each `next_chunk()` call.
pub struct FrameAdapter { /* ... */ }

impl FrameAdapter {
    /// Creates a new adapter with an empty frame.
    /// Call `update()` to set the initial frame before streaming.
    pub fn new() -> Self;

    /// Sets the pending frame.
    ///
    /// The frame becomes current at the start of the next `next_chunk()` after
    /// a wrap. If called multiple times before a swap, only the most recent
    /// frame is kept (latest-wins).
    pub fn update(&mut self, frame: Frame);

    /// Must always return exactly `req.n_points` points.
    pub fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint>;

    /// Returns a thread-safe handle for updating frames from another thread.
    pub fn shared(self) -> SharedFrameAdapter;
}

/// Thread-safe handle for updating frames from another thread.
#[derive(Clone)]
pub struct SharedFrameAdapter { /* ... */ }

impl SharedFrameAdapter {
    pub fn update(&self, frame: Frame);
    pub fn next_chunk(&self, req: &ChunkRequest) -> Vec<LaserPoint>;
}
```

The expected usage is:

- Blocking: `let req = stream.next_request()?; points = adapter.next_chunk(&req); stream.write(&req, &points)?;`
- Callback: `stream.run(move |req| Some(adapter.next_chunk(&req)), |_| {});`

**FrameAdapter behavior**

- Latest-wins: `update()` sets the pending frame; multiple calls before a swap keep only the most recent.
- Wrap-boundary swaps: the pending frame becomes current only at wrap boundaries (when the point index cycles back to 0), ensuring clean transitions without mid-cycle jumps.
- Chunks are produced by cycling through the frame's points (integer index, no resampling).
- Empty frames output blanked points at the last known position.
- For time-varying animation, use the streaming API directly with a point generator instead of FrameAdapter.

## Internal architecture (target layering)

Keep responsibilities separated:

1. **backends/** (per device)
   - Connect/disconnect
   - Convert `&[LaserPoint]` to on-wire/device formats
   - Expose `Caps`
   - Implement `try_write_chunk(pps, points)` with uniform “would block”
   - Optional: best-effort status (queue estimate)

2. **stream/** (scheduler/session)
   - Own pacing, buffering, and `StreamInstant`
   - Enforce caps and config validation
   - Maintain target queued points
   - Apply underrun policy when producer can’t keep up

3. **adapters/** (frames and other sources)
   - Frame adapters / resampling
   - Optional helpers (e.g. repeat-last, simple blank/park generators)

## Uniform backpressure contract (non-negotiable)

All backends must implement a meaningful “would block” outcome, even if the transport itself doesn’t report it:

- If the device provides real backpressure (Helios readiness, Ether Dream fullness), map that to `WouldBlock`.
- If the transport accepts writes “too easily” (e.g. UDP send succeeds), the stream layer must still pace output using the configured PPS and target queue.

Without a library-owned queue target, backends that are “always writable” can cause the producer to run far ahead, increasing latency and breaking the “real-time” feel of audio-reactive visuals.

The stream scheduler must never rely on “device busy exists” to avoid runaway; it must be able to pace using only time + configured queue targets. In this model:

- `scheduled_ahead_points` is the canonical pacing signal (library-owned, always meaningful).
- `device_queued_points` is informational only and may be absent or unreliable depending on the DAC/protocol.

## What this enables

- Stable, bounded-latency XY oscilloscope/vectorscope output across DACs
- Cross-device consistent audio-reactive behavior (FFT, envelope, beat/onset)
- Debuggable streams: stream time + queue depth make behavior observable
- Multi-DAC setups without per-device timing heuristics in the application
- Clear separation of concerns: engines generate chunks; the library guarantees pacing and device constraints

## Pro-grade considerations (target story)

These are not required to begin the refactor, but they are important for “pro dependency” expectations (multi-projector rigs and show control).

### Multi-device sync (shared epoch)

`StreamInstant` gives a shared *unit* (“points”), but multi-projector rigs also need a shared *epoch* so multiple devices stay phase-aligned.

Target direction:

- Provide a way to start multiple streams on a common epoch (and recover from reconnect/drift) so DAC A and DAC B are both rendering “chunk starting at t=123456” at the same time.
- This could be expressed as a coordinator/group API (exact shape TBD), e.g. a `StreamGroup` that owns an epoch and produces per-device `ChunkRequest`s on that shared timeline.

### Latency contract + instrumentation

For audio-reactive visuals, the library should make it easy to keep latency bounded and observable:

- Treat `scheduled_ahead_points` as the canonical queue-depth signal for pacing.
- Expose queue depth and basic counters continuously via `status()` (underruns, late chunks, reconnects).
- Optionally add a lightweight telemetry hook for UIs (effective PPS, underrun events, reconnect reasons).

### “WouldBlock” stays internal in blocking mode

In blocking mode, callers should not normally need to handle `WouldBlock`: `next_request()` should wait until the stream is ready (or fail with `Disconnected` / `Backend(...)`). `WouldBlock` remains important as a backend contract and for potential future non-blocking APIs.

## Compatibility stance

This refactor does not preserve legacy/worker-centric APIs. The crate is still early, and downstream codebases are expected to adapt to the streaming-first API.
