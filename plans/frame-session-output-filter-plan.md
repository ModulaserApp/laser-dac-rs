# FrameSession Output Filter Plan

## In Short

This plan adds an optional `output_filter` hook to `FrameSession` that runs on
the final point sequence immediately before it is written to the DAC.

The filter sees the real presented output after transitions, frame-swap
clamping, blanking, and color delay, so downstream apps can apply final
output-space processing without taking over scheduling or maintaining separate
FIFO and frame-swap pipelines.

## Summary

Add a stateful output-filter hook to `FrameSession` so callers can transform or
validate the exact point sequence that will be written to hardware.

The hook is intended for downstream consumers such as Modulaser that need
additional output-space processing after frame composition, after transition
insertion, after frame-swap capacity clamping, after arm/startup blanking, and
after color delay, but before backend write.

This preserves the frame-first architecture for all DACs, including frame-swap
backends such as Helios and ShowNET, without forcing downstream apps to
re-implement scheduling or maintain separate FIFO and frame-swap pipelines.

## Architecture Decision

This plan assumes the architectural decision has been made:

- `FrameSession` is the canonical API for frame-producing output on both FIFO
  and frame-swap DACs
- `start_stream()` remains the FIFO-native callback API
- `laser-dac-rs` will not emulate streaming on top of frame-swap DACs such as
  Helios or ShowNET

This is important because the output-filter hook is not a stopgap. It is the
intended extension point for downstream applications that need final
output-space processing without taking ownership of presentation or scheduling.

Related note: reconnect-aware custom discovery is no longer a blocker for this
work. `open_device_with(...)` and `Dac::with_discovery_factory(...)` already
exist in the crate, so this plan focuses only on the final presented-output
hook and its companion liveness requirements.

## Problem

`FrameSession` already owns the presentation-critical logic:

- latest-wins frame replacement
- self-loop and A->B seam composition via `TransitionFn`
- FIFO pacing and frame-swap scheduling
- frame-swap capacity clamping
- startup blanking and disarm blanking
- color delay state
- reconnect state replay

Downstream applications may still need safety-critical output processing on the
actual presented point sequence. Running that processing on the authored frame
before `send_frame()` is not exact, because the library may later:

- insert transition points
- coalesce seam points
- prepend self-loop transition points on frame-swap DACs
- truncate transition prefixes to fit frame-swap hardware limits
- apply startup/disarm blanking
- apply color delay
- retry the already-composed output buffer verbatim on `WouldBlock`

For frame-swap DACs this is especially important: the device loops the exact
buffer submitted via `write_frame()`, so any output-space processing should see
that final hardware frame, not the pre-composition authored frame.

## Goal

Expose a hook in `FrameSession` that lets callers process the final output slice
immediately before backend write, while preserving:

- one frame-based API for all DAC classes
- identical hook semantics for FIFO and frame-swap once a logical frame has
  been materialized
- zero exceptions for Helios / ShowNET
- exact retry behavior
- reconnect replay behavior

## Non-Goals

- Do not replace `TransitionFn`
- Do not add a second scheduling API
- Do not move scheduling responsibility out of `FrameSession`
- Do not redesign the callback streaming API
- Do not add frame-swap support to `start_stream()`
- Do not emulate streaming semantics on top of frame-swap hardware

## Rejected Alternative

The main alternative considered was adding frame-swap support to the callback
streaming API by emulating streaming on top of frame-replacement hardware such
as Helios and ShowNET.

That approach was rejected for this plan.

Why:

- frame-swap DACs are modeled honestly in the crate as `FrameSwapBackend`s, not
  as FIFO devices
- `start_stream()` currently has coherent FIFO-native semantics:
  `ChunkRequest`, `target_buffer`, `min_points`, and `buffered_points` all mean
  something real for queue-driven backends
- emulating streaming on top of frame-swap hardware would make those semantics
  partly synthetic and backend-specific
- the complexity would not replace `FrameSession`; it would add a second
  presentation path that has to fake buffering, readiness, retry, and hold-last
  behavior for hardware that natively accepts whole frames

For long-term architecture, it is better to keep frame-swap DACs on the
frame-first API and add the missing final-output hook there.

## Proposed API

Use a small stateful trait instead of a plain closure. The filter may need
persistent state across FIFO chunks or frame-swap frame submissions, and it
needs explicit reset semantics for output continuity breaks.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputResetReason {
    SessionStart,
    Reconnect,
    Arm,
    Disarm,
}

pub trait OutputFilter: Send + 'static {
    fn reset(&mut self, reason: OutputResetReason) {}

    fn filter(&mut self, points: &mut [LaserPoint], ctx: &OutputFilterContext);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentedSliceKind {
    FifoChunk,
    FrameSwapFrame,
}

#[derive(Debug, Clone, Copy)]
pub struct OutputFilterContext {
    pub pps: u32,
    pub kind: PresentedSliceKind,
    pub is_cyclic: bool,
}
```

Add it to `FrameSessionConfig`:

```rust
pub struct FrameSessionConfig {
    pub pps: u32,
    pub transition_fn: TransitionFn,
    pub startup_blank: Duration,
    pub color_delay_points: usize,
    pub reconnect: Option<ReconnectConfig>,
    pub output_filter: Option<Box<dyn OutputFilter>>,
}

impl FrameSessionConfig {
    pub fn with_output_filter(
        mut self,
        filter: Box<dyn OutputFilter>,
    ) -> Self {
        self.output_filter = Some(filter);
        self
    }
}
```

Why a trait and not `FnMut`:

- explicit reset semantics are required at output continuity boundaries
- clearer semantics for stateful processing
- easier to extend later than a raw closure type alias

Retry contract:

- the filter runs once per newly materialized presented slice
- `WouldBlock` retries reuse the already-filtered buffer verbatim
- the filter is not re-run for the same materialized slice

That contract is more useful than exposing an `is_retry` flag that should almost
always be false.

## Exact Execution Order

The key requirement is: the filter must see the exact sequence that would
otherwise be written to hardware.

### FIFO path

Current order in `run_fifo_estimation_loop()` and `run_udp_timed_loop()` is:

1. `engine.fill_chunk(...)`
2. `apply_blanking(...)`
3. `color_delay.apply(...)`
4. `backend.try_write(...)`

Proposed order:

1. `engine.fill_chunk(...)`
2. `apply_blanking(...)`
3. `color_delay.apply(...)`
4. `output_filter.filter(...)`
5. `backend.try_write(...)`

Special case before the first frame:

- FIFO currently emits blank-origin keepalive chunks before any frame has been
  submitted.
- Frame-swap currently emits nothing until `compose_hardware_frame()` returns a
  non-empty frame.
- To preserve hook parity, the output filter must **not** run on pre-first-frame
  FIFO keepalive blanks.
- The first filter invocation for both delivery modes must happen only after a
  logical frame has been submitted and materialized into a presented slice.

Context for the filter:

- `kind = PresentedSliceKind::FifoChunk`
- `is_cyclic = false`

On `WouldBlock`, the already-filtered buffer must be retried verbatim. Do not
re-run the filter during retry spins.

### Frame-swap path

Current order in `run_frame_swap_loop()` is:

1. `engine.compose_hardware_frame()`
2. copy into `frame_buf`
3. `apply_blanking(...)`
4. `color_delay.apply(...)`
5. `backend.try_write(...)`

Important detail: `compose_hardware_frame()` already includes:

- A->B transition composition
- self-loop composition
- frame-swap transition-prefix clamping to hardware capacity

Proposed order:

1. `engine.compose_hardware_frame()`
2. copy into `frame_buf`
3. `apply_blanking(...)`
4. `color_delay.apply(...)`
5. `output_filter.filter(...)`
6. `backend.try_write(...)`

Context for the filter:

- `kind = PresentedSliceKind::FrameSwapFrame`
- `is_cyclic = true`

This gives the filter the actual looped hardware frame, after transition
prefix truncation. That is the critical property needed by downstream safety
systems.

## Why This Works For Frame-Swap DACs

Frame-swap DACs are the strongest case for this design.

The backend does not consume a scheduler-owned rolling chunk stream. It loops
the exact buffer passed to `write_frame()`. Therefore the correct object for
output-space processing is the final composed hardware frame:

- transition prefix included
- coalesce decisions already applied
- capacity clamp already applied
- startup/disarm blanking already applied
- color delay already applied

The filter can then treat the slice as a cyclic loop and process the final
`last -> first` seam correctly.

## Reset Semantics

Call `OutputFilter::reset(reason)` whenever the output continuity is known to be
broken by session state changes. At minimum:

- session startup before the first logical frame is materialized
- reconnect success in `try_reconnect(...)`
- color-delay reset on reconnect
- `engine.reset()` on reconnect
- every arm/disarm state transition

Do not automatically reset on every new frame. A downstream filter may need
state continuity across FIFO chunks or across repeated frame-swap writes of the
same logical output.

Arm/disarm must not be left implementation-defined. The contract should be:

- disarm is a continuity break because the shutter state changes and the output
  stream becomes forced-blanked
- re-arm is a second continuity break because startup blanking is injected
  before visible content resumes
- therefore the filter resets on both edges using distinct reasons

After the reset, the filter still runs on the presented output as usual once a
logical frame exists. That means disarmed blanked output and post-arm startup
blanking are still visible to the filter, but they are seen from a fresh state.

## Retry Semantics

Retry behavior must remain exact.

For both FIFO and frame-swap paths:

- materialize buffer once
- run blanking / color delay
- run output filter once
- retry the same resulting buffer until accepted or disconnected

This avoids double-applying stateful filters on `WouldBlock` loops.

## Reconnect Semantics

On reconnect:

1. reconnect backend
2. reset engine
3. replay `last_frame` into the engine
4. reset color delay
5. reset output filter with `OutputResetReason::Reconnect`

This mirrors existing replay behavior and ensures downstream filters do not
carry stale state across transport discontinuities.

## Related Work: Final-Output Liveness

Downstream applications such as Modulaser may need a watchdog that monitors
activity from the final output path rather than from authored-frame submission.

That work is intentionally split into a separate plan:

- [frame-session-liveness-plan.md](/Users/rutger/Dev/laser-dac-rs/plans/frame-session-liveness-plan.md)

The important boundary for this plan is:

- `OutputFilter` is not the watchdog signal
- filter invocation counts are not a sufficient liveness source because retries
  reuse an already-filtered buffer and pre-first-frame FIFO keepalive blanks do
  not invoke the filter
- the filter API should not be distorted just to carry watchdog state

## Modulaser Parity Targets

This plan is intended to support Modulaser migration without losing important
behavioral guarantees from its current output path.

The implementation should preserve parity for:

- seam handling:
  self-loop seams and A->B seams must be filtered on the exact presented
  sequence, including frame-swap transition-prefix clamping
- retry behavior:
  `WouldBlock` must retry the same already-filtered slice verbatim
- continuity resets:
  reconnect and arm/disarm edges must reset filter continuity explicitly
- liveness:
  downstream watchdog support is required, but tracked separately from this
  feature because it is not an `OutputFilter` responsibility
- first-frame behavior:
  pre-first-frame FIFO keepalive blanks must not invoke the filter, but they
  still matter for the separate liveness design
- color-delay ordering:
  the filter must run after color delay so downstream safety sees the true
  hardware-bound color/intensity sequence

These parity targets do not require preserving Modulaser's current callback API.
They require preserving the final observable behavior that matters for safety
and debugging.

## File-Level Implementation Plan

### 1. `src/presentation/mod.rs`

- Add public `OutputFilter`, `OutputFilterContext`, `PresentedSliceKind`, and
  `OutputResetReason`
- Re-export them from `lib.rs`

### 2. `src/presentation/session.rs`

- Add `output_filter: Option<Box<dyn OutputFilter>>` to `FrameSessionConfig`
- Add `with_output_filter(...)`
- Thread the filter into all scheduler loops
- Run the filter after blanking and color delay, before backend write
- Ensure retry loops reuse the already-filtered buffer
- Call `reset(reason)` on startup, reconnect, arm, and disarm

### 3. `src/lib.rs`

- Re-export new public types
- Update crate-level examples if needed

### 4. `README.md`

- Document the new hook as an advanced frame-mode extension point
- State clearly that it runs on the final presented sequence
- State the difference between `FifoChunk` and `FrameSwapFrame`
- State clearly that `start_stream()` remains FIFO-only and is not supported on
  frame-swap DACs

## Testing Plan

Add focused tests in `src/presentation/tests.rs`.

### API / ordering tests

- filter is called for FIFO frame mode
- filter is not called for pre-first-frame FIFO keepalive blanks
- filter is called for frame-swap mode
- filter runs after color delay
- filter runs after startup/disarm blanking
- frame-swap filter sees post-clamp frame, not pre-clamp frame

### FIFO estimation loop tests

- `run_fifo_estimation_loop()` does not re-run the filter inside the inner
  `WouldBlock` retry loop
- `run_fifo_estimation_loop()` calls the filter again only after a new chunk is
  materialized

### UDP-timed FIFO loop tests

- `run_udp_timed_loop()` does not re-run the filter while `retry_points`
  preserves the same transmit buffer across outer-loop iterations
- `run_udp_timed_loop()` calls the filter again only when a new chunk is
  materialized

### Frame-swap retry tests

- frame-swap `WouldBlock` does not re-run filter for the same materialized frame

### Reset tests

- session startup triggers filter reset
- reconnect triggers filter reset
- replayed `last_frame` after reconnect flows through filter again
- disarm transition triggers filter reset
- re-arm transition triggers filter reset before startup blanking/content resume
- reset reasons are correct for startup / reconnect / arm / disarm

### Semantics tests

- frame-swap filter receives `is_cyclic = true`
- FIFO filter receives `is_cyclic = false`
- frame-swap filter sees self-loop transition points
- frame-swap filter sees A->B transition prefix
- FIFO filter sees already-delayed colors
- both FIFO schedulers and frame-swap share the same first-invocation contract:
  no callback before the first logical frame is materialized

### Regression tests for exactness

Build test filters that stamp or count invocations:

- `CountingFilter` to verify call counts
- `TaggingFilter` to mutate intensity/RGB in a detectable way
- `RecordingFilter` to capture slice lengths and context

## Rollout Strategy

1. Land the API and reset semantics first with tests
2. Update docs and examples
3. Keep it opt-in: no behavior change for callers that do not install a filter
4. Downstream projects can then migrate frame-mode safety/output processing onto
   the new hook without changing scheduler ownership

## Expected Downstream Use

For Modulaser, the intended use is:

- keep generating internal `LaserFrame`s upstream
- keep using `TransitionFn` for seam blanking policy
- move output-space safety onto `FrameSessionConfig::with_output_filter(...)`
- use the same frame-based path for FIFO, Helios, and ShowNET

That gives Modulaser one frame-first architecture while still letting its
output-space processing observe the real presented sequence.

## Open Questions

### Should the filter run before or after color delay?

Recommendation: after color delay.

Reason: the filter should see the actual point/color/intensity sequence written
to hardware. If a downstream system cares about energy or lit-state, delayed
colors are part of the real output.

### Should the filter be able to reject output?

Recommendation: no. Keep it transform-only for the first version.

Reason: rejection complicates retry semantics and fail-dark behavior. A caller
that wants fail-dark can blank or repair points in-place.

### Should the filter receive more context?

For the first version, keep `OutputFilterContext` small, but do not underspecify
continuity semantics. The reset reason is part of the public contract now.

Additional context can still be added later if needed:

- `frame_index`
- `transition_prefix_len`
- `armed` state
- reconnect/discontinuity markers beyond reset reason

## Recommendation

Implement the output-filter hook now, as a small, well-tested extension point in
`FrameSession`.

This keeps the frame-first architecture intact, solves the frame-swap case
cleanly, and gives downstream applications an exact place to process the final
presented sequence without taking ownership of scheduling or transport details.
