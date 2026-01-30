# Streaming API Refactor Progress

This checklist tracks progress on implementing the streaming API refactor as described in `streaming-api-refactor-proposal.md`.

## Task 1: Core Types (`src/types.rs`)

- [x] **1.1 Update `StreamInstant`** - Add `as_secs_f64(pps: u32) -> f64` method and update docs (commit 88ab801)
- [x] **1.2 Create `FillRequest` struct** - New struct with `start`, `pps`, `min_points`, `target_points`, `buffered_points`, `buffered`, `device_queued_points` (commit ea36160)
- [x] **1.3 Create `FillResult` enum** - `Filled(usize)`, `Starved`, `End` variants (commit 2ad8147)
- [x] **1.4 Remove `ChunkRequest`** - Delete old struct and update usages (commit 3e0a353)
- [x] **1.5 Update `StreamConfig`** - Add `tick_interval`, `target_buffer`, `min_buffer` fields (commit 124af43)

## Task 2: Stream Internals (`src/stream.rs`)

- [x] **2.1 Update callback signature** - Add `run_fill()` with `FnMut(&FillRequest, &mut [LaserPoint]) -> FillResult`
- [x] **2.2 Add pre-allocated buffers to `Stream`** - `chunk_buffer` and `last_chunk` sized to `caps.max_points_per_chunk` (commit bba5174)
- [x] **2.3 Implement fixed-tick timing loop** - Use `sleep_until(next_tick)` with tick overrun handling (implemented in `run_fill()`)
- [x] **2.4 Implement buffer state calculation** - Calculate `buffered_points`, `buffered`, `playhead`, `min_points`, `target_points` (commit 9040195)
- [x] **2.5 Implement conservative buffer estimation** - Use `min(hardware, software)` estimation (commit b75858f)
- [x] **2.6 Implement `FillResult` handling** - Handle `Filled(n)`, `Starved`, `End` variants (implemented in `run_fill()`)
- [x] **2.7 Update `Stream::write()` method** - Replaced by `write_fill_points()` internal method in `run_fill()` API

## Task 3: Session Updates (`src/session.rs`)

- [x] **3.1 Update session callback signature** - Match new callback signature (commit 3e0a353)

## Task 4: FrameAdapter Updates (`src/frame_adapter.rs`)

- [x] **4.1 Update `FrameAdapter::fill_chunk()` method** - Rename from `next_chunk()`, change signature (commit 54ebdfa)
- [x] **4.2 Update `SharedFrameAdapter`** - Update `fill_chunk()` to match new signature (commit 840a509)
- [x] **4.3 Update FrameAdapter tests** - Convert to use `FillRequest` and `FillResult` (commit 3e0a353)

## Task 5: Example Updates (`examples/`)

- [x] **5.1 Update `callback.rs` example** - Use new callback signature (commit 3e0a353)
- [x] **5.2 Update `stream.rs` example** - Use new `FillRequest` type (commit 3e0a353)
- [x] **5.3 Update other examples** - Audit and update all examples (commit 3e0a353)

## Task 6: Test Updates

- [x] **6.1 Update existing stream tests** - Convert to new callback signature (commit 3e0a353)
- [x] **6.2 Add timing loop tests** - Test fixed-tick behavior, tick overrun handling, early wake
- [ ] **6.3 Add buffer estimation tests** - Test conservative estimation
- [ ] **6.4 Add `FillResult` handling tests** - Test all variants
- [ ] **6.5 Add integration tests** - Test full stream lifecycle

## Task 7: Documentation

- [x] **7.1 Update module-level docs** - Document new timing model (commit 3e0a353)
- [x] **7.2 Update README examples** - Show new callback signature (commit 3e0a353)

## Task 8: Final Verification

- [x] **8.1 Run all tests** - `cargo test` passes (commit 3e0a353)
- [x] **8.2 Run all examples** - Each example compiles and runs (commit 3e0a353)
- [x] **8.3 Run clippy** - `cargo clippy` passes (commit 3e0a353)
- [ ] **8.4 Verify documentation** - `cargo doc` builds without warnings
