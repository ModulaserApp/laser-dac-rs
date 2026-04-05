# FrameSession Liveness Plan

## In Short

This plan adds a small liveness surface for `FrameSession` so downstream
applications can implement an independent watchdog that detects wedged output
threads without depending on `OutputFilter` invocation counts.

The immediate downstream user is Modulaser, which currently has an independent
watchdog thread that polls DAC output activity and emergency-disarms a DAC if
its output path stops making progress while armed.

## Why This Is Separate From `output_filter`

`OutputFilter` and watchdog liveness are related but not the same feature.

They should stay separate because:

- the filter is an output-transformation hook, not a scheduler/transport status
  API
- retries reuse an already-filtered buffer, so filter invocation count is not a
  reliable liveness signal
- pre-first-frame FIFO keepalive blanks should not invoke the filter, but they
  still matter for output-thread liveness
- folding watchdog concerns into the filter contract would make the filter API
  less clear and harder to keep minimal

## Problem

`FrameSession` owns the actual output thread for both:

- FIFO delivery loops in `src/presentation/session.rs`
- frame-swap delivery loops in `src/presentation/session.rs`

Downstream watchdogs need a way to answer:

- is this armed DAC session thread still alive?
- is it still progressing through wait / retry / write logic?
- has the output path wedged badly enough that we should emergency-disarm?

Today Modulaser answers that by watching callback-return activity in its own DAC
thread model. After migrating to `FrameSession`, that old signal disappears.

## Goal

Expose or maintain a small per-session liveness surface that:

- is updated by the `FrameSession` scheduler thread itself
- remains meaningful for FIFO, UDP-timed, and frame-swap backends
- does not depend on visible content being present
- does not depend on `OutputFilter`
- can be consumed by a downstream independent watchdog thread

## Non-Goals

- Do not move watchdog policy into `laser-dac-rs`
- Do not make `laser-dac-rs` own operator-facing arming policy
- Do not add safety-disarm logic directly to `FrameSession`
- Do not expose a huge observability API in the first version

The downstream application still owns:

- watchdog polling cadence
- stall thresholds
- emergency-disarm policy

## Proposed Shape

Use a lightweight metrics/status handle associated with each `FrameSession`.

Possible public shape:

```rust
#[derive(Clone)]
pub struct FrameSessionMetrics {
    inner: Arc<FrameSessionMetricsInner>,
}

impl FrameSessionMetrics {
    pub fn connected(&self) -> bool;
    pub fn last_loop_activity(&self) -> Option<Instant>;
    pub fn last_write_success(&self) -> Option<Instant>;
}
```

`FrameSession` would expose:

```rust
impl FrameSession {
    pub fn metrics(&self) -> FrameSessionMetrics;
}
```

If `Instant` exposure is awkward for the public API, an alternative is to store
monotonic timestamps internally and expose elapsed durations.

## What Should Count As Liveness

The key signal is output-thread progress, not content visibility.

Good liveness events:

- entering a scheduler loop iteration
- progressing through sleep slices while armed
- progressing through ready/would-block retry loops
- successful backend writes
- reconnect-loop progress

Bad liveness signals:

- number of authored frames submitted
- number of `OutputFilter` invocations
- presence of lit points

An armed DAC that is outputting blanks safely is still alive.
A wedged scheduler thread is not.

## Semantics By Backend Class

### FIFO / UDP-timed

The metrics should remain alive even when:

- the buffer is healthy and the session is sleeping
- the producer has not submitted a new logical frame yet
- UDP-timed retry logic is reusing a previously materialized chunk

### Frame-swap

The metrics should remain alive even when:

- the backend is not yet ready for a new frame
- the session is polling readiness
- the session is retrying an already-composed hardware frame after
  `WouldBlock`

This matters because frame-swap DACs can be healthy while not accepting a new
frame at that instant.

## Recommended Downstream Use

For Modulaser, the intended design is:

- keep the watchdog thread independent
- keep roughly the same policy shape as today
- poll `FrameSessionMetrics` for each armed DAC
- emergency-disarm if the session has not shown loop activity for longer than
  the configured stall threshold

The watchdog should watch scheduler-thread liveness, not filter calls and not
pipeline frame-submission freshness.

## File-Level Implementation Plan

### 1. `src/presentation/session.rs`

- Add shared metrics state owned by the `FrameSession` thread
- Update liveness timestamps during:
  - loop iterations
  - long sleep slices
  - retry loops
  - reconnect progress
  - successful writes
- Expose a read-only metrics handle from `FrameSession`

### 2. `src/lib.rs`

- Re-export the metrics/status type if it is public

### 3. `README.md` or docs

- Document that `FrameSession` exposes a liveness surface for downstream
  watchdogs
- State clearly that watchdog policy remains downstream-owned

## Testing Plan

Add focused tests in `src/presentation/tests.rs`.

- metrics are available from `FrameSession`
- loop activity advances while FIFO session is sleeping with a healthy buffer
- loop activity advances while UDP-timed session retries the same chunk
- loop activity advances while frame-swap session waits for readiness
- write-success timestamp updates only on successful backend writes
- reconnect progress updates liveness rather than appearing stalled

## Open Questions

### Should this be public API?

Probably yes, because downstream watchdogs need to consume it.

If a fully public API feels too heavy, the first version could still expose a
small public read-only handle with only the minimum required fields.

### Should `last_loop_activity` and `last_write_success` both exist?

Probably yes.

They answer different questions:

- `last_loop_activity` answers "is the session thread alive?"
- `last_write_success` answers "when did the backend last accept output?"

For watchdog purposes, `last_loop_activity` is the primary signal.

## Recommendation

Implement a minimal `FrameSession` metrics/liveness handle as a separate
follow-on to the `output_filter` work.

This keeps the filter feature focused while still preserving the independent
watchdog model required by downstream safety-critical applications such as
Modulaser.
