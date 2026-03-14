# Frame-First Architecture

## Why this exists

`laser-dac-rs` currently centers its public model around chunk streaming:

- `Stream::run()` asks producers for `req.target_points`
- `FrameAdapter` is a simple point cycler
- frame-native DACs such as Helios are adapted into the same scheduler as FIFO DACs

This is not the right long-term architecture.

The Helios artifact investigation showed two different problems:

1. A concrete Helios USB transport bug, now fixed.
2. A deeper architectural mismatch: frame-swap DACs are being driven by deficit-sized top-up writes, which can produce tiny replacement frames and visible speckles.

At the same time, downstream applications like Modulaser already need a richer frame traversal model (`PathChunkRenderer`) to handle:

- self-loop seam blanking
- frame-to-frame transition blanking
- same-point seam suppression with tolerance
- pending-frame switching

That logic belongs in the library, not in each application.

## Core conclusion

The ideal `laser-dac-rs` should be:

- frame-first at the high-level API
- transport-specialized internally
- still capable of true low-level streaming for streaming-native DACs and expert use cases

This means the library should expose both:

1. A high-level frame/presentation API
2. A low-level streaming/chunk API

But the frame/presentation API should become the default, recommended path.

More importantly, transport differences should not be hidden behind one "generic chunk write" abstraction. Rust gives us the chance to encode the truth in the type system instead of relying on comments and implicit contracts.

## Native DAC classes

The library should explicitly model DACs by transport behavior, not just by protocol name.

### 1. Frame-swap DACs

Examples:

- Helios
- ShowNET

Properties:

- the device effectively accepts whole presentation frames
- a new frame replaces the currently queued or unsent one
- arbitrary tiny top-up chunks are the wrong transport model
- "streaming" is still possible, but it is quantized into stable frames

### 2. FIFO DACs

Examples:

- Ether Dream-style controllers
- LaserCube USB/WiFi
- likely other queue-driven devices

Properties:

- the device wants continuous top-up
- chunk streaming is the natural transport model
- frame playback can be adapted on top of streaming

### 3. Timed/audio-like transports

Examples:

- AVB and any future callback/audio-derived outputs

Properties:

- transport timing is distinct from point queue semantics
- may need their own scheduling policy

## Alternatives considered

### Option A: Minimal patch only

Add preferred/minimum frame sizes for frame-swap DACs and adjust scheduler behavior.

Pros:

- fast
- lower risk

Cons:

- leaves frame presentation logic outside the library
- preserves chunk-first architecture
- not a long-term "ground truth" design

### Option B: Add a presentation layer while preserving the current stream engine

Make frame presentation first-class and keep chunk streaming as an internal/expert path.

Pros:

- clean enough
- incremental migration
- preserves FIFO strengths
- lets apps stop reinventing `PathChunkRenderer`

Cons:

- medium refactor
- temporary coexistence of old and new abstractions

### Option C: Full frame-only transport everywhere

Force all DACs into frame delivery semantics.

Pros:

- simple mental model

Cons:

- wrong for FIFO DACs
- likely regresses Ether Dream/LaserCube-style behavior
- loses a major strength of streaming-native hardware

### Option D: Type-split delivery plus first-class presentation engine

This is the stronger version of Option B.

Characteristics:

- frame-first public API
- low-level streaming API retained
- immutable presentation artifacts
- FIFO and frame-swap delivery separated at the trait/scheduler boundary

Pros:

- expresses truth in the type system
- avoids implicit transport contracts
- cleaner long-term foundation than "one trait plus branches"
- gives the crate a chance to be genuinely better than existing libraries

Cons:

- larger refactor
- requires touching core traits and scheduler structure
- more migration work up front

## Chosen strategy

Option D is the recommended sustainable plan.

It is the best balance of:

- correct abstraction
- transport honesty
- long-term elegance
- type-level correctness

If the crate were already stable, this would be a harder sell.
Because the crate is early, this is exactly the right moment to do it properly instead of accumulating DAC-specific scheduler hacks.

---

## Open decisions

These two questions require a deliberate decision. A direction is proposed for each but they are marked explicitly as open.

### Decision 1: Point color representation

**Proposed direction**: Move to `f32` colors (0.0–1.0) for the public `LaserPoint` type.

Rationale:

- The presentation layer needs to synthesize blanking ramps, fades, and transitions. Float arithmetic is natural; integer arithmetic is awkward.
- libera-core uses floats and it works well in practice.
- Conversion to device-native formats (u8, u12, u16) happens once per backend, at the write boundary.
- The crate is pre-1.0. This is the last good window for this change.

```rust
pub struct LaserPoint {
    pub x: f32,          // -1.0 to 1.0
    pub y: f32,          // -1.0 to 1.0
    pub r: f32,          // 0.0 to 1.0
    pub g: f32,          // 0.0 to 1.0
    pub b: f32,          // 0.0 to 1.0
    pub intensity: f32,  // 0.0 to 1.0
}
```

Migration: search-and-replace `65535` → `1.0`, `0` → `0.0` in color fields. Provide `LaserPoint::new()` and `LaserPoint::blanked()` with the same signatures (just f32). Backends update their conversion helpers (already isolated in `coord_to_u12` etc.).

**Alternative**: Keep `u16` colors. Add `f32` convenience constructors. The presentation layer works in `f32` internally and converts at its own boundary. This avoids a breaking change but creates two representations in the crate.

### Decision 2: Scanner sync placement

**Proposed direction**: Dual-path.

- **Frame API**: Color delay is baked into `PresentationFrame` during construction. The presentation engine shifts RGB channels relative to XY within the frame's point buffer. Frame-swap DACs receive already-correct data with no runtime FIFO needed.
- **Streaming API**: The existing runtime color delay line remains, applied in `write_fill_points` as today. This is the right model for truly continuous procedural sources where there is no discrete frame to precompute.

This means `StreamConfig::color_delay` stays for streaming mode, and `SeamConfig` (or a broader `PresentationConfig`) gains a `color_delay_points` field for frame mode.

**Alternative**: Keep color delay as runtime-only for both paths. Simpler, but frame-swap DACs get cross-frame color bleed (the first N points of a new frame inherit colors from the previous frame's tail, which is the current libera-core behavior).

---

## Trait design

### Current state

```rust
// One trait for everything:
pub trait StreamBackend: Send + 'static {
    fn dac_type(&self) -> DacType;
    fn caps(&self) -> &DacCapabilities;
    fn connect(&mut self) -> Result<()>;
    fn disconnect(&mut self) -> Result<()>;
    fn is_connected(&self) -> bool;
    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome>;
    fn stop(&mut self) -> Result<()>;
    fn set_shutter(&mut self, open: bool) -> Result<()>;
    fn queued_points(&self) -> Option<u64> { None }
}
```

### Proposed split

```rust
/// Shared lifecycle and device identity for all backends.
pub trait DacBackend: Send + 'static {
    fn dac_type(&self) -> DacType;
    fn caps(&self) -> &DacCapabilities;
    fn connect(&mut self) -> Result<()>;
    fn disconnect(&mut self) -> Result<()>;
    fn is_connected(&self) -> bool;
    fn stop(&mut self) -> Result<()>;
    fn set_shutter(&mut self, open: bool) -> Result<()>;
}

/// FIFO-style backend: accepts variable-size point batches.
///
/// Used by Ether Dream, LaserCube USB, LaserCube WiFi, IDN, AVB.
pub trait FifoBackend: DacBackend {
    /// Attempt to write a batch of points.
    ///
    /// Returns `WouldBlock` when the device buffer is full.
    /// The batch size is chosen by the scheduler based on buffer estimation.
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome>;

    /// Best-effort estimate of points currently queued in the device.
    fn queued_points(&self) -> Option<u64> {
        None
    }
}

/// Frame-swap backend: accepts complete, fixed-size frames.
///
/// Used by Helios (and future ShowNET).
pub trait FrameSwapBackend: DacBackend {
    /// Maximum number of points the device accepts per frame.
    fn frame_capacity(&self) -> usize;

    /// Whether the device is ready to accept a new frame.
    ///
    /// For Helios this maps to `GetStatus() == Ready`.
    fn is_ready_for_frame(&self) -> bool;

    /// Write a complete frame to the device.
    ///
    /// The caller guarantees `points.len() <= frame_capacity()`.
    /// Returns `WouldBlock` if `is_ready_for_frame()` is false.
    fn write_frame(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome>;
}
```

### Backend classification

| Backend | Trait | Rationale |
|---|---|---|
| Helios | `FrameSwapBackend` | `WriteFrameExtended` is inherently frame-based |
| Ether Dream | `FifoBackend` | TCP point streaming with buffer top-up |
| LaserCube USB | `FifoBackend` | USB bulk transfer, continuous top-up |
| LaserCube WiFi | `FifoBackend` | UDP point streaming with ack flow control |
| IDN | `FifoBackend` | UDP streaming protocol |
| AVB | `FifoBackend` | Audio callback, continuous sample feed |
| (future) ShowNET | `FrameSwapBackend` | Frame-swap device |

### Unifying at the Dac level

Discovery and `Dac` need to hold either backend type. An enum does this:

```rust
pub(crate) enum BackendKind {
    Fifo(Box<dyn FifoBackend>),
    FrameSwap(Box<dyn FrameSwapBackend>),
}

pub struct Dac {
    info: DacInfo,
    backend: Option<BackendKind>,
}
```

The public API on `Dac` does not expose `BackendKind`. Both `start_stream()` and `start_frame_session()` work with either variant — the scheduler handles the difference internally.

---

## Module layout

```
src/
    lib.rs                          # public re-exports
    error.rs                        # Error, Result (unchanged)
    types.rs                        # LaserPoint, StreamConfig, DacType, etc.
    discovery.rs                    # device discovery (unchanged)
    net_utils.rs                    # network helpers (unchanged)

    backend/
        mod.rs                      # DacBackend trait, BackendKind enum, re-exports
        fifo.rs                     # FifoBackend trait
        frame_swap.rs               # FrameSwapBackend trait

    stream/
        mod.rs                      # Stream, Dac, public API
        control.rs                  # StreamControl (extracted from current stream.rs)
        scheduler.rs                # buffer estimation, sleep/pace logic
        fill.rs                     # write_fill_points, underrun handling, color delay

    presentation/
        mod.rs                      # PresentationEngine, public frame session API
        frame.rs                    # AuthoredFrame, PresentationFrame
        seam.rs                     # SeamConfig, blanking/transition computation
        traversal.rs                # cursor-based traversal for FIFO delivery

    session.rs                      # ReconnectingSession (updated for both modes)

    protocols/                      # per-DAC implementations (structure unchanged)
        helios/
        ether_dream/
        idn/
        lasercube_usb/
        lasercube_wifi/
        avb/
```

Key changes from current layout:

- `backend.rs` → `backend/` module with the trait split
- `stream.rs` (142KB) → `stream/` module, split into control, scheduler, and fill logic
- New `presentation/` module for the frame engine
- `frame_adapter.rs` deprecated (replaced by `presentation/`)

---

## Presentation types

### AuthoredFrame

The user-facing frame type. Contains only the visible authored points — no blanking, no seam handling.

```rust
/// A frame of authored laser content.
///
/// Contains only the visible points the user intends to draw.
/// The library handles loop seams, transitions, and blanking automatically
/// when this frame is submitted to a session.
pub struct AuthoredFrame {
    points: Vec<LaserPoint>,
}

impl AuthoredFrame {
    pub fn new(points: Vec<LaserPoint>) -> Self { ... }
    pub fn points(&self) -> &[LaserPoint] { ... }
    pub fn len(&self) -> usize { ... }
    pub fn is_empty(&self) -> bool { ... }
}

impl From<Vec<LaserPoint>> for AuthoredFrame { ... }
```

### PresentationFrame

An immutable, precomputed artifact ready for delivery. Contains the authored points plus any computed blanking/transition points for correct looping.

```rust
/// Immutable presentation artifact computed from authored content.
///
/// Contains the authored points plus computed seam/transition blanking,
/// ready to be delivered to hardware or traversed into chunks.
///
/// This is computed once when the frame changes, not on every callback.
pub struct PresentationFrame {
    /// The full point sequence (authored + computed blanking/transitions).
    points: Vec<LaserPoint>,
    /// Total number of points in the presentation.
    len: usize,
}

impl PresentationFrame {
    /// Build a looping presentation frame.
    ///
    /// Analyzes the authored frame's first/last points and inserts
    /// appropriate loop-return blanking based on the seam config.
    pub fn build_looping(
        frame: &AuthoredFrame,
        config: &SeamConfig,
    ) -> Self { ... }

    /// Build a transition from one frame to another.
    ///
    /// Produces: tail of `from` + transition blanking + head of `to`.
    /// Used when the presentation engine switches frames.
    pub fn build_transition(
        from: &AuthoredFrame,
        to: &AuthoredFrame,
        config: &SeamConfig,
    ) -> Self { ... }

    /// The full point sequence.
    pub fn points(&self) -> &[LaserPoint] { ... }

    /// Total presentation length in points.
    pub fn len(&self) -> usize { ... }
}
```

### SeamConfig

```rust
/// Configuration for seam and transition behavior.
pub struct SeamConfig {
    /// Distance threshold below which seam blanking is suppressed.
    ///
    /// If the distance between the last point and the first point
    /// is less than this value, no loop blanking is inserted.
    /// Uses Euclidean distance in normalized coordinate space.
    pub same_point_tolerance: f32,

    /// Number of blank points to insert at loop seams.
    ///
    /// These are blanked points that travel from the last authored point
    /// back to the first, giving the galvos time to reposition.
    pub loop_blank_count: usize,

    /// Number of blank points for frame-to-frame transitions.
    ///
    /// Inserted when switching from one frame to a different frame.
    pub transition_blank_count: usize,
}

impl Default for SeamConfig {
    fn default() -> Self {
        Self {
            same_point_tolerance: 0.01,
            loop_blank_count: 8,
            transition_blank_count: 12,
        }
    }
}
```

---

## PresentationEngine

The engine manages current/pending frames and drives delivery to either backend type.

```rust
/// The presentation engine manages frame lifecycle and delivery.
///
/// It holds the current and pending frames, computes presentation
/// artifacts, and feeds the appropriate delivery path.
pub(crate) struct PresentationEngine {
    /// The current presentation frame (looping).
    current: Option<Arc<PresentationFrame>>,
    /// The authored frame that `current` was built from.
    current_authored: Option<Arc<AuthoredFrame>>,
    /// Pending frame submitted by the user (latest-wins).
    pending: Option<Arc<AuthoredFrame>>,
    /// Traversal cursor for FIFO delivery.
    cursor: usize,
    /// Seam configuration.
    seam_config: SeamConfig,
}

impl PresentationEngine {
    pub fn new(seam_config: SeamConfig) -> Self { ... }

    /// Submit a new frame. Latest-wins semantics.
    pub fn submit(&mut self, frame: AuthoredFrame) { ... }

    /// Try to swap to the pending frame if one exists.
    ///
    /// Called at frame boundaries (cursor wrapped) or when the
    /// scheduler explicitly checks for pending content.
    ///
    /// Returns true if a swap occurred.
    pub fn try_swap(&mut self) -> bool { ... }

    /// Get the current presentation frame for frame-swap delivery.
    ///
    /// Returns None if no frame has been submitted yet.
    pub fn current_frame(&self) -> Option<&PresentationFrame> { ... }

    /// Fill a buffer with points from the current presentation.
    ///
    /// For FIFO delivery: traverses the presentation frame from the
    /// cursor position, wrapping around and checking for pending swaps
    /// at frame boundaries.
    ///
    /// Returns the number of points written.
    pub fn fill_chunk(&mut self, buffer: &mut [LaserPoint], max_points: usize) -> usize { ... }
}
```

---

## Scheduler design

The current `Stream::run()` loop needs to branch on backend type. Rather than one monolithic loop with branches, the scheduler becomes two focused implementations.

### FIFO scheduler (for FifoBackend)

Largely the same as the current `Stream::run()` loop:

1. Estimate buffer state (time-decay or `queued_points()`)
2. Sleep if buffer is above target
3. Build `ChunkRequest` with `min_points` / `target_points`
4. Call producer (user callback or `PresentationEngine::fill_chunk`)
5. Apply arm/blank/color-delay post-processing
6. Call `backend.try_write_points()`, handle `WouldBlock` with yield+retry

### Frame-swap scheduler (for FrameSwapBackend)

A different cadence:

1. Check `backend.is_ready_for_frame()`
2. If not ready, sleep briefly and retry
3. If ready:
   a. Ask the `PresentationEngine` for the current `PresentationFrame`
   b. Pad or truncate to `backend.frame_capacity()` if needed
   c. Apply arm/blank post-processing to the frame buffer
   d. Call `backend.write_frame()`
4. After write, check for pending frame swap

For streaming mode on a frame-swap backend (quantized streaming):
- The scheduler calls the user callback with `target_points == frame_capacity()`
- The callback fills exactly one frame's worth of points
- The scheduler sends it via `write_frame()`

---

## Public API

### Frame mode (recommended path)

```rust
use laser_dac::{open_device, AuthoredFrame, FrameSession, FrameSessionConfig, LaserPoint};

// 1. Open and configure
let device = open_device("helios:ABC123")?;
let config = FrameSessionConfig::new(30_000);

// 2. Start frame session
let session = device.start_frame_session(config)?;
session.control().arm()?;

// 3. Submit frames
let circle = make_circle_points();
session.send_frame(AuthoredFrame::new(circle));

// 4. Update content (latest-wins, swaps at frame boundary)
loop {
    let frame = generate_next_frame();
    session.send_frame(AuthoredFrame::new(frame));
    std::thread::sleep(Duration::from_millis(16)); // ~60fps
}
```

### FrameSession

```rust
/// Configuration for a frame session.
pub struct FrameSessionConfig {
    pub pps: u32,
    pub seam: SeamConfig,
    pub startup_blank: Duration,
}

impl FrameSessionConfig {
    pub fn new(pps: u32) -> Self { ... }
    pub fn with_seam(mut self, seam: SeamConfig) -> Self { ... }
}

/// A running frame presentation session.
///
/// Owns the backend and drives delivery. Submit frames via `send_frame()`.
/// The session handles looping, seam blanking, and transport-appropriate
/// delivery automatically.
pub struct FrameSession { ... }

impl FrameSession {
    /// Get the control handle (arm/disarm/stop).
    pub fn control(&self) -> &SessionControl { ... }

    /// Submit a new frame for presentation.
    ///
    /// The frame becomes pending immediately. It will be presented:
    /// - **Frame-swap DACs**: when the device signals readiness for the next frame
    /// - **FIFO DACs**: when the current frame's traversal completes
    ///
    /// Multiple calls before a swap keep only the most recent (latest-wins).
    pub fn send_frame(&self, frame: AuthoredFrame) { ... }

    /// Check if the session is ready to accept a new frame.
    ///
    /// For frame-swap DACs, this reflects hardware readiness.
    /// For FIFO DACs, this is always true (the pending slot is always available).
    pub fn is_ready(&self) -> bool { ... }

    /// Block until the session ends (stop requested or disconnect).
    pub fn join(self) -> Result<RunExit> { ... }
}
```

### Streaming mode (expert/FIFO path)

The existing API stays, with the scheduler internally using the correct backend trait.

```rust
use laser_dac::{open_device, StreamConfig, ChunkRequest, ChunkResult, LaserPoint};

let device = open_device("etherdream:aa:bb:cc:dd:ee:ff")?;
let config = StreamConfig::new(30_000);
let (stream, _info) = device.start_stream(config)?;

stream.control().arm()?;

let exit = stream.run(
    |req: &ChunkRequest, buffer: &mut [LaserPoint]| {
        // For FIFO backends: target_points varies based on buffer state
        // For frame-swap backends: target_points == frame_capacity (quantized)
        let n = req.target_points;
        for i in 0..n {
            buffer[i] = generate_point(req.start + i as u64, req.pps);
        }
        ChunkResult::Filled(n)
    },
    |err| log::error!("Stream error: {}", err),
)?;
```

When `start_stream()` is used with a frame-swap backend, the scheduler transparently quantizes:
- `ChunkRequest::target_points` is always `frame_capacity()`
- `ChunkRequest::min_points` is always `frame_capacity()`
- The callback must produce exactly that many points
- This is documented as "quantized streaming" behavior

### Both modes from one Dac

```rust
impl Dac {
    /// Start a frame presentation session (recommended).
    ///
    /// Works with all backend types. The session handles transport
    /// differences internally.
    pub fn start_frame_session(self, config: FrameSessionConfig) -> Result<FrameSession> { ... }

    /// Start a low-level streaming session (expert).
    ///
    /// Works with all backend types. For frame-swap backends,
    /// streaming is quantized to frame_capacity()-sized chunks.
    pub fn start_stream(self, config: StreamConfig) -> Result<(Stream, DacInfo)> { ... }
}
```

---

## FrameSession internals

`FrameSession` owns a thread that runs the appropriate scheduler.

```rust
// Simplified internal structure
pub struct FrameSession {
    control: SessionControl,
    thread: Option<std::thread::JoinHandle<Result<RunExit>>>,
    frame_tx: Sender<AuthoredFrame>,
}
```

The thread's main loop, for a FIFO backend:

```
loop:
    1. check stop / process control messages
    2. drain frame_tx → engine.submit()
    3. estimate buffer
    4. if buffer < target:
        engine.fill_chunk(buffer, target_points)
        apply arm/blank
        backend.try_write_points(buffer)
    5. sleep/pace
```

For a frame-swap backend:

```
loop:
    1. check stop / process control messages
    2. drain frame_tx → engine.submit()
    3. if backend.is_ready_for_frame():
        engine.try_swap()
        frame = engine.current_frame()
        apply arm/blank to frame copy
        pad to frame_capacity() if needed
        backend.write_frame(frame)
    4. else: sleep 1ms
```

---

## ReconnectingSession integration

`ReconnectingSession` currently wraps `Stream::run()`. It should be extended to also support frame sessions:

```rust
impl ReconnectingSession {
    /// Run with a frame source instead of a streaming callback.
    ///
    /// On disconnect, the session reconnects and resumes presenting
    /// the last submitted frame.
    pub fn run_frame_session(
        self,
        config: FrameSessionConfig,
        on_error: impl FnMut(Error) + Send + 'static,
    ) -> Result<(FrameSessionHandle, SessionControl)> { ... }
}

/// Handle for sending frames to a reconnecting frame session.
pub struct FrameSessionHandle {
    frame_tx: Sender<AuthoredFrame>,
}

impl FrameSessionHandle {
    pub fn send_frame(&self, frame: AuthoredFrame) { ... }
}
```

The `SessionControl` survives reconnections as it does today — arm/disarm state persists across reconnects.

---

## Blanking behavior

Blanking between frames must be part of presentation preparation, not transport improvisation.

### Self-loop case

If a frame repeats itself:

- if last point and first point are far apart, append blanking/travel points
- if they are effectively the same point, suppress seam blanking

So the repeated presentation frame already contains the right loop behavior.

### Frame switch case

If current frame A transitions to frame B:

- if A end and B start are far apart, append blanking/travel points
- if they are effectively the same point, suppress transition blanking

So frame switching is still visually correct in frame-swap mode.

This preserves Modulaser's current seam logic while moving it into the library.

---

## Migration path

### Phase 1: Trait split + frame-swap scheduling

**Backend implementors** (internal to crate):

Each protocol backend changes from `impl StreamBackend for XxxBackend` to either `impl FifoBackend for XxxBackend` or `impl FrameSwapBackend for XxxBackend`. The method signatures are nearly identical to today — the main change is renaming `try_write_chunk` to `try_write_points` or `write_frame`.

Example for Helios:

```rust
// Before:
impl StreamBackend for HeliosBackend {
    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        // check status, write frame
    }
}

// After:
impl DacBackend for HeliosBackend {
    // connect, disconnect, stop, set_shutter, dac_type, caps — unchanged
}

impl FrameSwapBackend for HeliosBackend {
    fn frame_capacity(&self) -> usize { 4095 }

    fn is_ready_for_frame(&self) -> bool {
        self.dac.as_ref()
            .and_then(|d| d.status().ok())
            .map(|s| s == DeviceStatus::Ready)
            .unwrap_or(false)
    }

    fn write_frame(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        // same body as current try_write_chunk, minus the status check
        // (is_ready_for_frame handles that)
    }
}
```

**Compatibility bridge**:

During Phase 1, a temporary `StreamBackend` adapter can wrap either backend type so the existing `Stream::run()` code path continues to work unchanged. This allows incremental migration without breaking the streaming API.

```rust
/// Temporary adapter: wraps a BackendKind as a StreamBackend
/// for backward compatibility during migration.
pub(crate) struct BackendAdapter {
    kind: BackendKind,
}

impl StreamBackend for BackendAdapter {
    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        match &mut self.kind {
            BackendKind::Fifo(b) => b.try_write_points(pps, points),
            BackendKind::FrameSwap(b) => {
                if !b.is_ready_for_frame() {
                    return Ok(WriteOutcome::WouldBlock);
                }
                b.write_frame(pps, points)
            }
        }
    }
    // ... delegate other methods
}
```

This adapter gets removed in Phase 3 when `Stream` is updated to use `BackendKind` directly.

**Public API impact**: None. `Dac::start_stream()` continues to work. `list_devices()` and `open_device()` are unchanged.

**Deliverable**: The type boundary becomes honest. Helios artifacts caused by tiny replacement writes are eliminated.

### Phase 2: Presentation engine

**New code only.** The `presentation/` module is added. No existing APIs change.

Modulaser can begin migrating `PathChunkRenderer` logic to use `PresentationFrame::build_looping()` and `SeamConfig` from the library.

**Deliverable**: Seam/transition behavior is owned by the library. Frame preparation is deterministic and reusable across DAC classes.

### Phase 3: Public frame API + retire FrameAdapter

**New API**: `Dac::start_frame_session()`, `FrameSession`, `FrameSessionConfig`.

**Deprecated**: `FrameAdapter`, `SharedFrameAdapter`. These are replaced by `FrameSession` which handles everything the adapter did plus seam handling, transport-correct delivery, and proper lifecycle management.

**Breaking change**: If Decision 1 (f32 colors) is taken, this is the release that ships it. All downstream code updates point construction.

**Modulaser migration**:

```rust
// Before (current):
let mut adapter = FrameAdapter::new();
let shared = adapter.shared();
stream.run(
    |req, buf| shared.fill_chunk(req, buf),
    |err| log::error!("{}", err),
)?;

// After (Phase 3):
let session = device.start_frame_session(config)?;
session.control().arm()?;
session.send_frame(AuthoredFrame::new(points));
```

Modulaser's `PathChunkRenderer` is replaced by the library's `PresentationEngine`. Seam tolerance, loop blanking, and transition blanking are configured via `SeamConfig` instead of custom application code.

**Deliverable**: Downstream apps can adopt the frame-first path without reimplementing `PathChunkRenderer`. The public API reflects the new architecture.

---

## What this design does better than libera-core

| Concern | libera-core (C++) | laser-dac-rs (proposed) |
|---|---|---|
| Transport correctness | Runtime branch on output model | Compile-time trait split |
| Frame preparation | Recomputed every callback | Immutable, computed once |
| Seam handling | Not in library (downstream) | Library-owned, configurable |
| Color delay in frame mode | Runtime FIFO, cross-frame bleed | Baked into presentation frame |
| Frame scheduling | Global static latency | Per-session configuration |
| Reconnection | Internal, not surfaced cleanly | `ReconnectingSession` with callbacks |
| Error contract | Assert + silent pad/truncate | Typed `ChunkResult` enum |
| Thread safety | Manual atomics + mutex | Ownership model, `Send` bounds |
| Stream lifecycle | Manual start/stop/join | `Stream` consumes `Dac`, RAII cleanup |

---

## Testing strategy

### Phase 1

- Unit tests for `BackendAdapter` compatibility bridge
- Existing `stream.rs` tests pass unchanged via adapter
- New tests: `FrameSwapBackend` mock that verifies:
  - `write_frame` is never called with fewer than `frame_capacity()` points
  - `write_frame` is never called when `is_ready_for_frame()` returns false

### Phase 2

- `PresentationFrame::build_looping` unit tests:
  - same-point tolerance suppresses blanking when endpoints match
  - blanking inserted when endpoints are far apart
  - correct point count (authored + blanking)
- `PresentationFrame::build_transition` tests:
  - transition blanking between dissimilar frames
  - suppression when endpoints are close
- `PresentationEngine::fill_chunk` tests:
  - correct traversal with cursor wrapping
  - pending frame swap at frame boundary
  - latest-wins semantics

### Phase 3

- Integration tests: `FrameSession` with mock FIFO and frame-swap backends
- Verify frame delivery cadence matches backend type
- `ReconnectingSession::run_frame_session` reconnection test
- Deprecation warnings on `FrameAdapter` usage

---

## Design goals

- A user should be able to stay frame-first and still get correct behavior on every DAC.
- A user should still be able to generate continuous streaming content if they need it.
- Helios and ShowNET should be honest quantized-frame devices, not faux FIFO devices.
- Applications should not need to reimplement seam handling and transition blanking.
- Backends should focus on hardware transport, not presentation semantics.
- The type system should make incorrect transport usage difficult rather than easy.

## Non-goals

- Forcing a single transport model on all DACs.
- Removing the low-level streaming API.
- Baking Modulaser-specific policy into the crate.

## Success criteria

This refactor is successful when:

- Helios/ShowNET no longer show tiny replacement-frame artifacts under normal use
- downstream apps do not need their own `PathChunkRenderer`-style infrastructure
- FIFO DACs still perform well for true continuous streaming use cases
- the public API makes the real DAC semantics easier to understand, not harder
- the type system makes incorrect transport usage difficult rather than easy
