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

The frame lifecycle and delivery logic (pending-frame switching, cursor traversal, just-in-time composition) belongs in the library. Transition blanking strategy is delegated to a user-supplied callback, with a simple default helper provided.

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
- base frame / drawable split: base cached, ending composed reactively
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

## Decisions

### Scanner sync placement

**Proposed direction**: Dual-path.

- **Frame API**: Color delay is applied when the drawable is composed. The presentation engine shifts RGB channels relative to XY within the composed point buffer. Frame-swap DACs receive already-correct data with no runtime FIFO needed.
- **Streaming API**: The existing runtime color delay line remains, applied in `write_fill_points` as today. This is the right model for truly continuous procedural sources where there is no discrete frame to precompute.

This means `StreamConfig::color_delay` stays for streaming mode, and `FrameSessionConfig` gains a `color_delay_points` field for frame mode.

**Alternative**: Keep color delay as runtime-only for both paths. Simpler, but frame-swap DACs get cross-frame color bleed (the first N points of a new frame inherit colors from the previous frame's tail).

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

/// Frame-swap backend: accepts complete frames up to a maximum size.
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
        frame.rs                    # AuthoredFrame
        transition.rs               # TransitionFn type alias, default_transition() helper

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

### The base frame / drawable split

A key design insight from Modulaser's `PathChunkRenderer`: the correct ending of a frame depends on what comes next, and we don't know what comes next when the frame is first submitted. This means we cannot fully precompute a "presentation frame" at submission time.

The solution is a two-stage model:

- **Base frame**: The authored content with no ending. Precomputed, cacheable, reusable. Ends at the last visible point.
- **Drawable**: The base frame plus a computed ending (self-loop blanking or transition blanking). Built just-in-time, when the engine knows what's actually coming next.

For FIFO DACs, the ending is stitched dynamically at the cursor wrap point — exactly like `PathChunkRenderer`. For frame-swap DACs, the ending is composed at send time, when the scheduler knows whether to self-loop or transition.

This is reactive, not precomputed. The ending is a just-in-time decision based on the current pending state.

### Why fully precomputed frames don't work for frame-swap DACs

Consider a triangle frame A looping on Helios. If we precompute blanking for self-loop (A_end → A_start), the following goes wrong:

- If frame B arrives before the next hardware swap: we baked blanking toward A_start, but we should have gone toward B_start. Wasted scan time going the wrong direction.
- If we precompute a transition (A_end → B_start), but B gets replaced by C before Helios is ready: the blanking points toward B_start, not C_start. Wrong destination.
- If we precompute a transition (A_end → B_start), but B isn't ready when Helios asks: we'd need to replay a frame whose ending goes toward B, but we need to loop A. Completely wrong.

The core tension: for frame-swap DACs, the entire frame is committed to hardware at once. The ending can't be changed mid-play. But the correct ending depends on future state that isn't known yet.

The solution: compose the complete hardware frame at the moment of sending, when we know exactly what's pending.

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
    pub fn first_point(&self) -> Option<&LaserPoint> { ... }
    pub fn last_point(&self) -> Option<&LaserPoint> { ... }
    pub fn len(&self) -> usize { ... }
    pub fn is_empty(&self) -> bool { ... }
}

impl From<Vec<LaserPoint>> for AuthoredFrame { ... }
```

### Transition callback

Transition handling is delegated to a user-supplied callback. The engine calls this function at every frame boundary (loop seam or frame switch) with the last point of the outgoing frame and the first point of the incoming frame. The callback returns the points to insert between them — or an empty vec for no transition.

```rust
/// Callback type for computing transition points between frames.
///
/// Called with the last point of the outgoing frame and the first point
/// of the incoming frame (which may be the same frame for self-loops).
/// Returns the points to insert between them. Return an empty vec
/// to suppress blanking entirely (e.g., for same-point seams).
pub type TransitionFn = Box<dyn Fn(&LaserPoint, &LaserPoint) -> Vec<LaserPoint> + Send>;
```

The crate ships a simple default helper:

```rust
/// Default transition: linearly interpolated blank points from A to B.
///
/// Always blanks, regardless of distance. For same-point detection,
/// custom dwell stages, or other advanced blanking strategies, provide
/// a custom `TransitionFn`.
pub fn default_transition(from: &LaserPoint, to: &LaserPoint) -> Vec<LaserPoint> {
    let count = 8;
    (0..count)
        .map(|i| {
            let t = (i + 1) as f32 / (count + 1) as f32;
            LaserPoint {
                x: from.x + (to.x - from.x) * t,
                y: from.y + (to.y - from.y) * t,
                r: 0.0, g: 0.0, b: 0.0, intensity: 0.0,
            }
        })
        .collect()
}
```

This keeps the crate simple. Applications like Modulaser that need richer blanking (same-point suppression, multi-stage dwell, galvo settling curves) provide their own callback — a natural 1:1 migration from `PathChunkRenderer::compute_point_transition()`.

---

## PresentationEngine

The engine manages current/pending frames and drives delivery to either backend type. It follows the base frame / drawable model from Modulaser's `PathChunkRenderer`: the base frame is cached, and the drawable (base + ending) is computed reactively based on what's pending.

```rust
/// The presentation engine manages frame lifecycle and delivery.
///
/// Follows a two-stage model:
/// - Base frame: the authored content, cached and reusable.
/// - Drawable: base frame + computed ending, built just-in-time when
///   the engine knows what comes next (self-loop or transition).
///
/// The ending is never baked in at submission time. It is always
/// computed at the moment of delivery, based on the current pending state.
/// Transition points are produced by a user-supplied callback.
pub(crate) struct PresentationEngine {
    /// The current base frame (authored content, no ending).
    current_base: Option<Arc<AuthoredFrame>>,
    /// Pending frame submitted by the user (latest-wins).
    pending_base: Option<Arc<AuthoredFrame>>,
    /// The current drawable: base frame content + computed ending.
    /// Recomputed when pending changes or when the frame wraps.
    drawable: Vec<LaserPoint>,
    /// Whether the drawable needs recomputation (pending changed).
    drawable_dirty: bool,
    /// Traversal cursor within the drawable.
    cursor: usize,
    /// User-supplied callback for computing transition points.
    transition_fn: TransitionFn,
}
```

### Key operations

```rust
impl PresentationEngine {
    pub fn new(transition_fn: TransitionFn) -> Self { ... }

    /// Submit a new frame. Latest-wins semantics.
    ///
    /// If no current frame exists, immediately promotes to current.
    /// Otherwise stores as pending. If a pending frame already exists,
    /// it is overwritten (latest-wins). Marks the drawable as dirty
    /// so the ending will be recomputed before the next delivery.
    pub fn set_pending(&mut self, frame: AuthoredFrame) { ... }

    /// Recompute the drawable from the current base frame.
    ///
    /// The ending is determined by the current pending state:
    /// - No pending: self-loop — calls `transition_fn(last, first)` for
    ///   the current frame and appends the result.
    /// - Pending B: transition — calls `transition_fn(last, B.first)`
    ///   and appends the result.
    ///
    /// The transition callback decides what points to insert (or none).
    fn refresh_drawable(&mut self) { ... }

    /// Compose a complete hardware frame for frame-swap delivery.
    ///
    /// Called by the frame-swap scheduler when the device is ready.
    /// Calls `transition_fn` based on what's pending right now,
    /// appends the result to the base frame content, and promotes
    /// the pending frame if one exists.
    ///
    /// Returns the composed point buffer ready for `write_frame()`.
    pub fn compose_hardware_frame(&mut self) -> &[LaserPoint] { ... }

    /// Fill a buffer with points for FIFO delivery.
    ///
    /// Traverses the drawable from the cursor position. At the wrap
    /// point (cursor reaches drawable end):
    /// - If pending exists: promote it, call `transition_fn` for the
    ///   transition, recompute drawable.
    /// - If no pending: call `transition_fn` for self-loop, reset cursor.
    ///
    /// Returns the number of points written.
    pub fn fill_chunk(&mut self, buffer: &mut [LaserPoint], max_points: usize) -> usize { ... }
}
```

### How scenarios play out

All scenarios call `transition_fn(from, to)` at the frame boundary. The only difference is what the target point is: the current frame's first point (self-loop) or the pending frame's first point (transition). The engine inserts whatever the callback returns.

**Self-loop, no pending**: Calls `transition_fn(A.last(), A.first())`. The callback decides what to insert (blanking points, or empty vec if same-point).

**Next frame ready**: Calls `transition_fn(A.last(), B.first())`. Inserts the result, promotes B to current.

**Frame skip (C replaces B before B is consumed)**: `set_pending()` overwrites B with C. Next boundary calls `transition_fn(A.last(), C.first())`. B is never drawn.

**Frame-swap DAC replays faster than updates arrive**: `compose_hardware_frame()` is called each time the device is ready. If no new pending has arrived, it composes the same base frame with self-loop transition. Recomputed each time (cheap) to ensure correctness.

**Pending arrives while frame-swap DAC is mid-play**: The currently-playing hardware frame is committed and can't be changed. When the device finishes and signals ready, `compose_hardware_frame()` sees the pending frame and composes a transition.

| Scenario | transition_fn call | Drawable ending |
|---|---|---|
| Self-loop (no pending) | `(A.last(), A.first())` | Callback result appended, then wrap |
| Next frame ready | `(A.last(), B.first())` | Callback result appended, B promoted |
| Frame skip (C pending) | `(A.last(), C.first())` | Callback result appended, C promoted |

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
   a. Drain pending frames from the channel
   b. Ask the `PresentationEngine` to compose a hardware frame
      (this computes the correct ending based on current pending state)
   c. Apply arm/blank post-processing
   d. Call `backend.write_frame()`

The hardware frame is composed at the moment of sending — not precomputed at submission time. This ensures the ending (self-loop or transition) always reflects the actual pending state, regardless of timing mismatches between the application's frame rate and the DAC's playback rate.

Frame sizing: the composed drawable (base + ending) is sent at its natural length. `frame_capacity()` is a maximum, not a target. Padding to capacity would change display time from `points.len() / pps` to `frame_capacity() / pps`, making short frames unacceptably slow. The hardware loops the frame at the given size.

For streaming mode on a frame-swap backend:
- The scheduler calls the user callback with `target_points` up to `frame_capacity()`
- The callback fills points as usual (partial fills are accepted)
- The scheduler sends whatever was produced via `write_frame()`

---

## Public API

### Frame mode (recommended path)

```rust
use laser_dac::{
    open_device, AuthoredFrame, FrameSession, FrameSessionConfig,
    LaserPoint, default_transition,
};

// 1. Open and configure
let device = open_device("helios:ABC123")?;
let config = FrameSessionConfig::new(30_000, default_transition);

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
    pub transition_fn: TransitionFn,
    pub startup_blank: Duration,
}

impl FrameSessionConfig {
    pub fn new(pps: u32, transition_fn: impl Fn(&LaserPoint, &LaserPoint) -> Vec<LaserPoint> + Send + 'static) -> Self { ... }
}

/// A running frame presentation session.
///
/// Owns the backend and drives delivery. Submit frames via `send_frame()`.
/// The session handles looping and transport-appropriate delivery
/// automatically. Transition blanking is computed by the user-supplied
/// `transition_fn` at each frame boundary.
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
        // For frame-swap backends: target_points up to frame_capacity
        let n = req.target_points;
        for i in 0..n {
            buffer[i] = generate_point(req.start + i as u64, req.pps);
        }
        ChunkResult::Filled(n)
    },
    |err| log::error!("Stream error: {}", err),
)?;
```

When `start_stream()` is used with a frame-swap backend:
- `ChunkRequest::target_points` is set up to `frame_capacity()` (the maximum the device accepts)
- The callback fills points as usual — partial fills are accepted, `Starved` and `End` work normally
- The scheduler sends whatever the callback produced via `write_frame()`, respecting the existing `ChunkResult` contract

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
    /// For frame-swap backends, target_points is up to frame_capacity().
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
    2. drain frame_tx → engine.set_pending()
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
    2. drain frame_tx → engine.set_pending()
    3. if backend.is_ready_for_frame():
        points = engine.compose_hardware_frame()
        apply arm/blank
        backend.write_frame(points)
    4. else: sleep 1ms
```

The critical difference: `compose_hardware_frame()` is called at the moment the device is ready, not when the frame was submitted. At that moment, the engine knows exactly what's pending and computes the correct ending (self-loop or transition). If the pending frame changed, was skipped, or hasn't arrived yet, the ending is always correct for the actual current state.

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

Blanking is delegated to the user-supplied `transition_fn` callback, called reactively at each frame boundary based on the current engine state. The engine never bakes transition points into frames at submission time — they are always computed at the moment of delivery.

The engine calls `transition_fn(from, to)` at every frame boundary:

- **Self-loop**: `transition_fn(current.last(), current.first())`
- **Transition**: `transition_fn(current.last(), pending.first())`

The callback returns `Vec<LaserPoint>` — the points to insert between frames. An empty vec means no transition points (useful for same-point seams or other cases where blanking is undesirable).

### Default behavior

The shipped `default_transition()` helper always generates linearly interpolated blank points from A to B. It does not detect same-point seams. Applications that need richer blanking (same-point suppression, multi-stage dwell, galvo settling curves) provide their own callback.

### Frame-swap DAC invariant

For frame-swap DACs, `transition_fn` is called at the moment the hardware signals readiness — not at frame submission time. This guarantees correctness regardless of timing mismatches between the application's update rate and the device's playback rate.

- If the DAC replays faster than updates arrive, each replay calls `transition_fn` for a self-loop. The transition always targets the current frame's start.
- If a pending frame arrives, the next composition calls `transition_fn` for the transition. The result goes from the current frame's endpoint to the pending frame's start.
- If a pending frame is overwritten before it plays (frame skip), `transition_fn` is called for the new pending frame. The skipped frame is never drawn.

### FIFO DAC behavior

For FIFO DACs, `transition_fn` is called at the cursor wrap point in `fill_chunk()`. When the cursor reaches the end of the drawable, the engine checks the pending state, calls `transition_fn` with the appropriate target, inserts the result, and continues filling the buffer.

### Before first frame

If no frame has been submitted yet, the engine has no `current_base`. Both `fill_chunk()` and `compose_hardware_frame()` should output blanked points at origin `(0, 0)` until the first frame arrives via `set_pending()`.

### Complete scenario matrix

All scenarios call `transition_fn(from, to)`. The only variables are the target point and whether a pending frame exists. Frame skip is not a special case — it is the natural result of latest-wins semantics.

| Scenario | DAC type | transition_fn call | What happens |
|---|---|---|---|
| A→A (self-loop) | FIFO | `(A.last, A.first)` | Result appended at cursor wrap, cursor resets |
| A→A (self-loop) | Frame-swap | `(A.last, A.first)` | Result appended, sent at natural size |
| A→B (transition) | FIFO | `(A.last, B.first)` | Result stitched at cursor wrap, B promoted |
| A→B (transition) | Frame-swap | `(A.last, B.first)` | Transition frame composed, B promoted, next frame is B self-loop |
| A→C (frame skip) | FIFO | `(A.last, C.first)` | B overwritten by `set_pending()`, transition goes A→C directly |
| A→C (frame skip) | Frame-swap | `(A.last, C.first)` | B overwritten, transition frame goes A→C, C promoted |

---

## Current vs proposed pipeline

### Current pipeline

1. `open_device()` → `Dac`
2. `Dac::start_stream(config)` → `Stream`
3. `Stream::run(callback)` — callback fills arbitrary-size chunks
4. All backends go through `try_write_chunk()` regardless of transport type
5. Frame users wrap a `FrameAdapter` into the streaming callback manually
6. Seam handling, blanking, frame transitions — all downstream responsibility
7. Frame-swap DACs (Helios) receive whatever chunk size the scheduler chose, leading to tiny replacement frames and visual artifacts

### Proposed pipeline (frame mode — recommended)

1. `open_device()` → `Dac`
2. `Dac::start_frame_session(config)` → `FrameSession`
3. `FrameSession::send_frame(AuthoredFrame)` — submit frames, latest-wins
4. `PresentationEngine` owns frame lifecycle; transition blanking is produced by the user-supplied `transition_fn` callback
5. Frame-swap backends: scheduler calls `compose_hardware_frame()` when device is ready, which calls `transition_fn` just-in-time, then sends via `write_frame()`
6. FIFO backends: scheduler calls `fill_chunk()` which traverses the drawable with cursor-based delivery, calling `transition_fn` at frame boundaries

### Proposed pipeline (streaming mode — expert)

1. `open_device()` → `Dac`
2. `Dac::start_stream(config)` → `Stream`
3. `Stream::run(callback)` — same callback API as today
4. FIFO backends: unchanged behavior, variable chunk sizes
5. Frame-swap backends: `target_points` up to `frame_capacity()`, callback fills as usual, sent via `write_frame()`

### What changes for backend implementors

Each protocol backend changes from `impl StreamBackend` to either `impl FifoBackend` or `impl FrameSwapBackend`. The method signatures are nearly identical — the main change is renaming `try_write_chunk` to `try_write_points` or `write_frame`, and for frame-swap backends, separating the readiness check into `is_ready_for_frame()`.

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

### What changes for downstream consumers (Modulaser)

`FrameAdapter` and `SharedFrameAdapter` are replaced by `FrameSession`. Modulaser's `PathChunkRenderer` frame lifecycle logic (pending frame management, cursor traversal, frame promotion) is replaced by the library's `PresentationEngine`. Modulaser's transition blanking logic migrates into a `TransitionFn` callback.

```rust
// Before (current):
let mut adapter = FrameAdapter::new();
let shared = adapter.shared();
stream.run(
    |req, buf| shared.fill_chunk(req, buf),
    |err| log::error!("{}", err),
)?;

// After:
let config = FrameSessionConfig::new(30_000, modulaser_transition);
let session = device.start_frame_session(config)?;
session.control().arm()?;
session.send_frame(AuthoredFrame::new(points));
```

Modulaser's transition callback would roughly look like:

```rust
/// Modulaser's transition callback — migrated from PathChunkRenderer.
///
/// Implements same-point suppression and multi-stage blanking
/// (dwell → transit → dwell) for clean galvo settling.
fn modulaser_transition(from: &LaserPoint, to: &LaserPoint) -> Vec<LaserPoint> {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let distance = (dx * dx + dy * dy).sqrt();

    // Same-point suppression: if endpoints match, no blanking needed
    if distance < SAME_POINT_TOLERANCE {
        return vec![];
    }

    let mut points = Vec::new();

    // Stage 1: end dwell — hold at `from` with laser off
    for _ in 0..END_DWELL_COUNT {
        points.push(LaserPoint::blanked(from.x, from.y));
    }

    // Stage 2: transit — interpolate from → to with laser off
    for i in 1..=TRANSIT_COUNT {
        let t = i as f32 / (TRANSIT_COUNT + 1) as f32;
        points.push(LaserPoint::blanked(
            from.x + dx * t,
            from.y + dy * t,
        ));
    }

    // Stage 3: start dwell — hold at `to` with laser off
    for _ in 0..START_DWELL_COUNT {
        points.push(LaserPoint::blanked(to.x, to.y));
    }

    points
}
```

### What stays the same

- `list_devices()`, `open_device()`, device discovery — unchanged
- `Dac::start_stream()` and `Stream::run()` — still available as expert/FIFO path
- `StreamControl` / `SessionControl` arm/disarm/stop model — unchanged
- `ReconnectingSession` — extended to support frame sessions, existing streaming usage unchanged
- Per-protocol implementations — same structure, different trait

---

---

## Testing strategy

### Trait split

- `FrameSwapBackend` mock that verifies:
  - `write_frame` is never called when `is_ready_for_frame()` returns false
  - scheduler sends frames at natural size (not padded to capacity)
- `FifoBackend` mock that verifies variable-size writes and `WouldBlock` backpressure
- Existing `stream.rs` tests continue to pass

### Presentation engine

- `default_transition` unit tests:
  - correct blank point count and interpolation
  - points are blanked (zero color/intensity)
- `PresentationEngine::fill_chunk` (FIFO delivery) tests:
  - correct cursor traversal with wrap
  - self-loop: transition_fn called with `(last, first)` at wrap point
  - pending frame swap: transition_fn called with `(last, pending.first)` at wrap
  - empty transition result (callback returns `vec![]`): no points inserted
  - latest-wins: pending overwritten before consumed
- `PresentationEngine::compose_hardware_frame` (frame-swap delivery) tests:
  - self-loop: transition_fn called when no pending
  - transition: transition_fn called when pending exists
  - pending replaced mid-play (frame skip) calls transition_fn for new pending
  - composed frame does not exceed frame_capacity

### Frame session

- Integration tests: `FrameSession` with mock FIFO and frame-swap backends
- Verify frame delivery cadence matches backend type
- `ReconnectingSession::run_frame_session` reconnection test

---

## Design goals

- A user should be able to stay frame-first and still get correct behavior on every DAC.
- A user should still be able to generate continuous streaming content if they need it.
- Helios and ShowNET should be honest quantized-frame devices, not faux FIFO devices.
- Applications should not need to reimplement frame lifecycle management (pending frames, cursor traversal, just-in-time composition).
- Backends should focus on hardware transport, not presentation semantics.
- The type system should make incorrect transport usage difficult rather than easy.

## Non-goals

- Forcing a single transport model on all DACs.
- Removing the low-level streaming API.
- Baking Modulaser-specific policy into the crate.

## Success criteria

This refactor is successful when:

- Helios/ShowNET no longer show tiny replacement-frame artifacts under normal use
- downstream apps do not need their own frame lifecycle infrastructure (pending management, cursor traversal)
- FIFO DACs still perform well for true continuous streaming use cases
- the public API makes the real DAC semantics easier to understand, not harder
- the type system makes incorrect transport usage difficult rather than easy
