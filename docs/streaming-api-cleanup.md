# Streaming API Cleanup Items

Post-refactor cleanup tasks identified during PR #3 review.

## Priority: High

### 0. Fix software-only buffer estimation in `run_fill`

**Location:** `src/stream.rs:714-722, 737-776`

When `StreamBackend::queued_points()` returns `None`, `FillRequest` buffer math is based on `scheduled_ahead`, but `scheduled_ahead` is only incremented and never decremented for elapsed playback time. This can cause `target_points` to drop to 0 permanently on backends without queue depth reporting.

- [x] Track elapsed playback and decrement software buffer estimate accordingly
- [x] Add/adjust tests to cover the `queued_points() == None` path end-to-end

### 1. Prevent division-by-zero for sub-millisecond `tick_interval`

**Location:** `src/stream.rs:444-447`

`run_fill()` computes missed ticks using `tick_interval.as_millis()`; for `tick_interval < 1ms`, `as_millis()` becomes 0 and can panic via division-by-zero (and `tick_interval == 0` breaks scheduling in other ways).

- [x] Validate `tick_interval` is non-zero and >= 1ms (or rework math to use nanos without truncation)
- [x] Add a regression test for `tick_interval < 1ms`

### 2. Fix `duration_millis` serde round-trip type mismatch

**Location:** `src/types.rs:476-488`

`duration_millis` serializes with `Duration::as_millis()` (`u128`) but deserializes as `u64`, which can break round-trips for typed/binary formats.

- [x] Serialize and deserialize using the same integer type (with bounds-checking/clamping as needed)
- [x] Add a serde round-trip test for `StreamConfig` when the `serde` feature is enabled

## Priority: Medium

### 3. Remove legacy `StreamConfig` fields

**Location:** `src/types.rs:465-468, 502-503, 544-555`

Remove deprecated fields that are no longer needed with the new timing model:

- [x] Remove `chunk_points: Option<usize>` field
- [x] Remove `target_queue_points: usize` field
- [x] Remove `with_chunk_points()` builder method
- [x] Remove `with_target_queue_points()` builder method
- [x] Update `Default` impl to remove these fields

### 4. Remove vestigial `Stream::chunk_points`

**Location:** `src/stream.rs:214, 229, 237, 262-264, 276`

The `chunk_points` field and accessor are no longer meaningful - the timing loop uses `max_points_per_chunk` from capabilities.

- [x] Remove `chunk_points` field from `Stream` struct
- [x] Remove `chunk_points()` accessor method
- [x] Remove from `Stream::with_backend()` signature
- [x] Update `Dac::start_stream()` to not compute/pass it

### 5. Remove legacy validation in `Dac::validate_config`

**Location:** `src/stream.rs:901-926`

- [x] Remove `chunk_points` validation (lines 909-919)
- [x] Remove `target_queue_points` validation (lines 921-923)

### 6. Remove `compute_default_chunk_size` function

**Location:** `src/stream.rs:928-945`

No longer used with the new timing model.

- [x] Remove the function entirely

---

## Priority: Low

### 7. Implement drain wait for `FillResult::End`

**Location:** `src/stream.rs:494-498`

The proposal specifies graceful shutdown should let queued points drain before returning. Currently it returns immediately.

```rust
FillResult::End => {
    // TODO: Implement drain wait in future PR
    return Ok(RunExit::ProducerEnded);
}
```

- [ ] Implement waiting for buffer to drain
- [ ] Add configurable drain timeout
- [ ] Park/blank laser after drain completes

### 8. Update doc comment referencing deprecated field

**Location:** `src/stream.rs:112`

```rust
/// `target_queue_points` bounds this latency.
```

- [x] Change to reference `target_buffer` instead

### 9. Update `StreamStatus` struct

**Location:** `src/types.rs:662-674`

- [x] Remove or repurpose `chunk_points` field
- [x] Consider adding `tick_interval` or other useful metrics

### 10. Update frame adapter doc examples

**Location:** `src/frame_adapter.rs:22-31, 76-85`

Doc examples reference the removed `next_request()`/`write()` API.

- [x] Update module-level doc example to use `run_fill()` with `fill_chunk()`
- [x] Update `FrameAdapter` struct doc example

---

## Verification

After cleanup:

- [x] `cargo test` passes
- [x] `cargo clippy` passes
- [x] `cargo doc --no-deps` builds without warnings
- [x] All examples compile and run
