# PR #3 Review: Buffer-Driven Timing with Zero-Allocation Callbacks

## Overview

Complete rewrite of the streaming API implementing buffer-driven timing with zero-allocation callbacks, based on research from five major laser DAC codebases.

## Key Changes

- Replaces `ChunkRequest` / `Option<Vec<LaserPoint>>` API with `FillRequest` / `FillResult` and library-owned buffers
- Removes blocking mode (`next_request()`/`write()`) in favor of unified `run_fill()` callback API
- Implements conservative buffer estimation using `min(hardware, software)` to prevent underruns
- Adds graceful drain on shutdown via `FillResult::End`
- Converts all examples and tests to the new API

## Code Quality

- Excellent documentation with clear explanations of the timing model and API contract
- Comprehensive test coverage with 128+ tests
- Clean separation of concerns with well-named helper methods
- Good use of Rust idioms (builder pattern, enums for result states)
- Consistent code style throughout

## Issues Fixed

1. **[FIXED] Simplify last_chunk copy** (`src/stream.rs`)
   - Removed redundant `.min()` call since buffers are pre-allocated to same size
   - Added `debug_assert!` to verify invariant

2. **[FIXED] Document NoQueueTestBackend pattern** (`src/stream.rs:1063-1073`)
   - Added doc comment explaining when to use `TestBackend` vs `NoQueueTestBackend`

3. **[FIXED] Decrease `scheduled_ahead` over time when `queued_points()` is `None`**
   - Added elapsed time tracking between loop iterations
   - `scheduled_ahead` now decrements based on wall-clock time even without sleeping
   - Prevents stalls on backends without `queued_points()` (e.g., Helios)

## Potential Risks

1. **Breaking change**
   - Removal of blocking mode is justified given alpha status
   - Should be clearly communicated in release notes

2. **Drain timeout default (1 second)**
   - Could delay shutdown if DAC stalls
   - Users with real-time requirements may want lower values
   - Well documented with `with_drain_timeout(Duration::ZERO)` option

## Performance

- Zero allocations in hot path (main goal achieved)
- Pre-allocated buffers sized to `max_points_per_chunk`
- In-place blanking when disarmed
- `copy_from_slice` for last_chunk storage

## Test Coverage

- Buffer estimation with/without hardware queue depth
- All `FillResult` variants (`Filled`, `Starved`, `End`)
- Underrun policies (`RepeatLast`, `Blank`, `Park`, `Stop`)
- Drain timeout behavior
- Full lifecycle tests
- Edge cases (`Filled(0)`, `Filled(>buffer.len())`)

## Security

- No concerns
- Proper bounds checking via `.min(buffer.len())` and `.min(max_points)`
- Clean shutdown with shutter close on all exit paths

## Verdict

**Approve** - All issues fixed. Ready to merge.
