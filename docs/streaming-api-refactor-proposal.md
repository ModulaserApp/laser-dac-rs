# Streaming API Refactor Proposal

## Executive Summary

This document proposes a **complete rewrite** of the `laser-dac-rs` streaming API to use **fixed timing with variable chunk sizes** and **zero-allocation callbacks**. This aligns with industry consensus from five major laser DAC codebases we researched.

**This is a clean break.** The crate is in alpha with zero users. We will ship a single PR that rewrites the streaming API entirely, with no backwards compatibility concerns. The goal is to get the API right before going public.

---

## Problem Definition

### Current Design

```rust
// Current callback signature
F: FnMut(ChunkRequest) -> Option<Vec<LaserPoint>>

// Current ChunkRequest
pub struct ChunkRequest {
    pub start: StreamInstant,
    pub pps: u32,
    pub n_points: usize,                 // FIXED per stream
    pub scheduled_ahead_points: u64,
    pub device_queued_points: Option<u64>,
}
```

**Current behavior:**
- **PPS**: Fixed for stream duration ✓
- **Chunk size**: Fixed for stream duration (computed at startup, ~10ms worth of points)
- **Timing between callbacks**: Variable (waits when buffer full, calls sooner when draining)
- **Memory**: Callback allocates new `Vec<LaserPoint>` every call, library clones for underrun recovery

### Problems with Current Design

1. **Variable callback timing makes audio sync difficult**
   - Audio systems use fixed-interval callbacks (e.g., every 5-20ms)
   - When laser callback timing varies, aligning with audio frame boundaries is complex
   - Applications must interpolate or buffer to handle timing mismatches

2. **Allocation per chunk**
   - New `Vec<LaserPoint>` allocated every callback (~100 calls/second)
   - Library clones points into `last_chunk` for underrun recovery
   - Allocator jitter is problematic for real-time applications

3. **Fixed chunk size limits adaptability**
   - Cannot request more points when buffer is critically low
   - Cannot request fewer points when buffer is full
   - Buffer health relies entirely on timing variance

---

## Industry Research

We analyzed five major laser DAC codebases to understand how they handle streaming.

### Summary Table

| Codebase | Timing Model | Chunk Size | PPS | Buffer Detection | Drift Correction |
|----------|--------------|------------|-----|------------------|------------------|
| **helios_openidn** | Fixed 15ms | Variable | Variable (speed factor) | ms calculation | ±20-30% speed factor |
| **helios_dac** | Poll-based | Variable | Per-frame | Binary (USB) | Adaptive packets |
| **ofxLaser** | Calculated per-cycle | Variable | Dynamic | Dual estimation | None |
| **etherdream-td** | Fixed sample rate | Variable | Fixed | Hardware reports | None |
| **ShowNET** | Push/poll (no callback) | Variable (frames) | Per-frame | Binary wait/ready | None |

### Key Finding

**All five codebases use variable chunk sizes.** None use fixed chunk size with variable timing (our current approach).

### Detailed Findings

#### helios_openidn

- **Timing**: Fixed 15ms intervals
- **Chunks committed when**: `currentSliceTime >= maxSliceDuration` (time-based, not point-count-based)
- **Buffer target**: 40ms
- **Drift correction**: Cubic error response adjusting speed factor 0.8x-1.3x

```cpp
double error = (center - bufUsageMs);
double offCenter = error * error * error / center;
speedFactor = std::min(1.3, std::max(0.80, newSpeed));
```

#### helios_dac

- **Timing**: Host polls `GetStatus()`, pushes when ready
- **PPS**: Can change per-frame via `WriteFrame(devNum, pps, ...)`
- **Buffer detection**: USB is binary (ready/not ready), IDN tracks sample counts
- **Recommended callback signature from maintainer**:

```cpp
size_t fill_callback(
    HeliosPoint* buffer,      // Pre-allocated buffer
    size_t max_points,        // Headroom available (variable)
    size_t min_points,        // Minimum needed to avoid underrun
    void* user_data
);
```

#### ofxLaser

- **Timing**: Dynamically calculated based on buffer state

```cpp
int pointsUntilNeedsRefill = MAX(minPacketDataSize, bufferFullness - minPointsInBuffer);
int microsToWait = pointsUntilNeedsRefill * (1000000.0f/pps);
microsToWait -= 30000; // 30ms safety buffer
usleep(microsToWait);
```

- **Buffer estimation**: Dual approach (time-since-sent vs time-since-acked)
- **Exception**: AVBSound/Dante DAC uses fixed chunk size because audio subsystem is authoritative (pull-based)

#### etherdream-touch-designer

- **Timing**: Fixed sample rate matching galvo PPS rating
- **Buffer detection**: Ether Dream hardware reports actual queue depth
- **Generator pattern**: Lazy evaluation prevents over-buffering

#### ShowNET (LaserWorld)

- **Model**: Synchronous push/poll (no callbacks)
- **API**: Application calls `WaitForLastFrame()` then `WriteFrame()`
- **PPS**: Configurable per-frame
- **Memory**: Caller-owned buffers (zero-copy)
- **No drift correction**: Relies on polling synchronization

```cpp
// Application allocates
ShowNet_RGB_Point_T points[500];
// Fill buffer, then pass pointer
ShowNet_WriteFrameSimple(dev, points, count, scanSpeed);
```

---

## Design Options

### Option A: Fixed Chunk Size, Variable Timing (Current)

```
Callback always requests N points
Timing varies based on buffer state
```

| Pros | Cons |
|------|------|
| Simple callback contract | Hard to sync with audio |
| Predictable compute per call | Cannot adapt to buffer emergencies |
| | No industry precedent |

### Option B: Fixed Timing, Variable Chunk Size (Proposed)

```
Callback invoked every ~10ms
Points requested varies based on buffer headroom
```

| Pros | Cons |
|------|------|
| Aligns with audio frame boundaries | Callback handles variable point counts |
| Naturally adapts to buffer state | Potential compute spikes (mitigated by max_points cap) |
| Industry consensus | |

### Option C: Synchronous Push/Poll (ShowNET style)

```
No callbacks
Application polls for ready, pushes frames
```

| Pros | Cons |
|------|------|
| Simplest API | Application must implement timing |
| Maximum flexibility | No managed streaming experience |
| Zero-copy natural | |

**Recommendation: Option B** - It's what all callback-based codebases use, and provides the best balance of simplicity and capability.

---

### Memory: Callback Allocates vs Library-Owned Buffer

#### Callback Allocates (Current)

```rust
F: FnMut(ChunkRequest) -> Option<Vec<LaserPoint>>
```

| Pros | Cons |
|------|------|
| Simple for prototypes | Allocation per chunk |
| Callback owns data | Allocator jitter |
| | Clone for underrun recovery |

#### Library-Owned Buffer (Proposed)

```rust
F: FnMut(&FillRequest, &mut [LaserPoint]) -> FillResult
```

| Pros | Cons |
|------|------|
| Zero allocations in hot path | Must fill provided buffer |
| Predictable memory usage | Slightly more complex callback |
| Cache-friendly | |

**Recommendation: Library-owned buffer** - All experts and the helios_dac maintainer recommended this approach.

---

### Return Type: Option vs FillResult Enum

#### Option (Current)

```rust
Option<Vec<LaserPoint>>
// Some(points) = data
// None = end stream
```

Problem: Cannot distinguish "I have no data right now" from "stream is finished."

#### FillResult Enum (Proposed)

```rust
pub enum FillResult {
    Filled(usize),  // Wrote n points
    Starved,        // No data available (apply underrun policy)
    End,            // Stream finished
}
```

**Recommendation: FillResult enum** - Cleaner underrun handling and explicit end-of-stream.

---

### Drift Correction: Speed Factor vs Variable Chunk Size

Some codebases (helios_openidn) adjust effective PPS ±20-30% to correct for host/device clock drift.

**Analysis:**

With variable chunk size:
- Device consuming faster than expected → buffer drains → request more points → buffer stabilizes
- `StreamInstant` represents **estimated** playback time (playhead + buffered), not guaranteed wall-clock time
- The estimate is accurate enough for audio sync when buffer estimation is conservative

**Important caveat:** `StreamInstant` provides *logical* stream time anchored to an estimated playhead. It's not a hardware-verified timestamp. For most audio-reactive applications, this is sufficient. For sample-accurate sync, an external clock mode would be needed.

**Conclusion:** Variable chunk size inherently handles drift. Speed factor is redundant complexity.

**Recommendation: No speed factor** - Variable chunk size is sufficient for buffer health. Applications needing tighter sync can use the future external clock mode.

---

## Proposed Design

### Core Types

#### StreamInstant Semantics

**Definition:** `StreamInstant` represents the **estimated playback time** of points, not merely "points sent so far."

```rust
/// Represents a point in stream time, anchored to estimated playback position.
///
/// `start` in FillRequest = playhead + buffered
///
/// Where:
/// - `playhead` = stream_epoch + estimated_consumed_points
/// - `buffered` = points sent but not yet played
///
/// This allows callbacks to generate content for the exact time it will be displayed,
/// enabling accurate audio synchronization.
pub struct StreamInstant(pub u64);

impl StreamInstant {
    /// Convert to seconds at the given PPS.
    pub fn as_secs_f64(&self, pps: u32) -> f64 {
        self.0 as f64 / pps as f64
    }
}
```

**Key invariant:** `req.start` tells the callback "generate content for this playback time." The callback can trust this for audio sync.

#### FillRequest

```rust
pub struct FillRequest {
    /// Estimated playback time when this chunk starts.
    /// Calculated as: playhead + buffered_points
    /// Use this for audio synchronization.
    pub start: StreamInstant,

    /// Points per second (fixed for stream duration)
    pub pps: u32,

    /// Minimum points needed to avoid imminent underrun.
    /// Calculated with ceiling to prevent underrun: ceil((min_buffer - buffered) * pps)
    /// If 0, buffer is healthy.
    pub min_points: usize,

    /// Ideal number of points to reach target buffer level.
    /// Calculated as: ceil((target_buffer - buffered) * pps), clamped to buffer.len()
    pub target_points: usize,

    // NOTE: max_points removed - use buffer.len() directly as the cap

    /// Current buffer level in points (for diagnostics/adaptive content)
    pub buffered_points: u64,

    /// Current buffer level as duration (for audio sync convenience)
    pub buffered: Duration,

    /// Raw device queue if available (best-effort, may differ from buffered_points)
    pub device_queued_points: Option<u64>,
}
```

**Invariant:** `buffer.len()` passed to the callback IS the maximum. No separate `max_points` field.

**Why two point tiers (not three)?**
- `min_points`: "We're about to underrun, please give us at least this many" (ceil to avoid underrun)
- `target_points`: "This would bring buffer to ideal level" (clamped to `buffer.len()`)
- `buffer.len()`: implicit maximum, no need to duplicate in request

**Rounding rules:**
- `min_points`: always **ceiling** (underrun prevention)
- `target_points`: **ceiling**, then clamped to `buffer.len()`

#### FillResult

```rust
pub enum FillResult {
    /// Wrote n points to the buffer.
    /// n must be <= buffer.len()
    /// If n < min_points, underrun policy is applied for the deficit.
    Filled(usize),

    /// No data available right now.
    /// Underrun policy is applied (repeat last chunk or blank).
    /// Stream continues; callback will be called again next tick.
    Starved,

    /// Stream is finished. Shutdown sequence:
    /// 1. Stop calling callback
    /// 2. Let queued points drain (play out)
    /// 3. Blank/park the laser at last position
    /// 4. Return from stream() with RunExit::ProducerEnded
    End,
}
```

**`Filled(0)` semantics:**
- If `target_points == 0`: Buffer is full, nothing needed. This is fine.
- If `target_points > 0`: We needed points but got none. Treated as `Starved`.

**`Starved` semantics:**
- Temporary condition (e.g., audio buffer not ready yet)
- Underrun policy fills the gap
- Callback will be called again next tick
- Does NOT end the stream

**`End` semantics:**
- Permanent condition (show is over)
- Queued points are allowed to drain (not discarded)
- Laser is safely parked after drain completes
- Stream returns gracefully

#### Callback Signature

```rust
F: FnMut(&FillRequest, &mut [LaserPoint]) -> FillResult
```

### StreamConfig

```rust
pub struct StreamConfig {
    /// Points per second (fixed for stream duration)
    pub pps: u32,

    /// How often to call the callback (default: 10ms)
    pub tick_interval: Duration,

    /// Target buffer level to maintain (default: 40ms)
    pub target_buffer: Duration,

    /// Minimum buffer before requesting urgent fill (default: 10ms)
    pub min_buffer: Duration,

    /// What to do when callback returns Starved or < min_points
    pub underrun_policy: UnderrunPolicy,
}
```

### Timing Loop

```rust
let mut next_tick = Instant::now();

loop {
    // 1. Wait for next tick (sleep_until to avoid drift from callback runtime)
    let now = Instant::now();
    if now < next_tick {
        std::thread::sleep(next_tick - now);
    }
    next_tick += config.tick_interval;

    // 2. Handle tick overrun (callback took longer than tick_interval)
    //    Skip missed ticks rather than running back-to-back
    if Instant::now() > next_tick {
        let missed = (Instant::now() - next_tick).as_millis() / config.tick_interval.as_millis();
        next_tick += config.tick_interval * (missed as u32 + 1);
        // Optionally: stats.missed_ticks += missed;
    }

    // 3. Calculate buffer state
    let buffered_points = estimate_buffer_points(); // conservative estimate
    let buffered = Duration::from_secs_f64(buffered_points as f64 / config.pps as f64);

    // 4. Calculate playhead for StreamInstant
    let playhead = stream_epoch + estimated_consumed_points;
    let start = StreamInstant(playhead + buffered_points);

    // 5. Calculate point requirements (ceiling for min to prevent underrun)
    let deficit_target = config.target_buffer.saturating_sub(buffered);
    let deficit_min = config.min_buffer.saturating_sub(buffered);

    let target_points = (deficit_target.as_secs_f64() * config.pps as f64).ceil() as usize;
    let min_points = (deficit_min.as_secs_f64() * config.pps as f64).ceil() as usize;

    let buffer_len = chunk_buffer.len().min(caps.max_points_per_chunk);
    let target_points = target_points.min(buffer_len);

    // 6. Call callback
    let req = FillRequest { start, pps, min_points, target_points, buffered_points, buffered, .. };
    let result = callback(&req, &mut chunk_buffer[..buffer_len]);

    // 7. Handle result
    match result {
        FillResult::Filled(n) => {
            write_to_dac(&chunk_buffer[..n]);
            scheduled_ahead += n as u64;
            // Update last_chunk for RepeatLast policy
            last_chunk[..n].copy_from_slice(&chunk_buffer[..n]);
            last_chunk_len = n;
        }
        FillResult::Starved => {
            apply_underrun_policy(); // Uses last_chunk or blanks
        }
        FillResult::End => {
            // Graceful shutdown: let queued points drain, then park
            wait_for_drain();
            park_laser();
            return RunExit::ProducerEnded;
        }
    }

    // 8. Early wake check (optional: wake before next tick if critically low)
    if buffered < config.min_buffer / 2 {
        next_tick = Instant::now(); // Wake immediately
    }
}
```

**Key timing details:**
- `sleep_until(next_tick)` not `sleep(tick_interval)` - prevents drift from callback runtime
- Tick overrun: skip missed ticks, don't run back-to-back (avoids cascade failure)
- Early wake: if buffer critically low, don't wait for next tick

### Buffer Estimation Strategy

Different DACs have different capabilities:

| DAC | Buffer Info | Strategy |
|-----|-------------|----------|
| Ether Dream | Hardware reports queue depth | Trust hardware value |
| Helios USB | Binary ready/not ready | Software estimate from time elapsed |
| IDN | Timestamps | Estimate from packet timing |
| LaserCube | Software tracking | Estimate from time elapsed |

```rust
fn estimate_buffer_points(&self) -> u64 {
    let software = self.software_estimate();

    // When hardware reports queue depth, use MINIMUM of hardware and software.
    // Using max would overestimate and cause underruns when hardware reports
    // less queued than we think.
    if let Some(device_queue) = self.backend.queued_points() {
        return device_queue.min(software);
    }

    software
}

fn software_estimate(&self) -> u64 {
    let elapsed = self.last_write_time.elapsed();
    let consumed = (elapsed.as_secs_f64() * self.config.pps as f64) as u64;
    self.scheduled_ahead.saturating_sub(consumed)
}
```

**Why `min` not `max`?**
- If hardware reports fewer points than software estimates, hardware is truth
- Using `max` would overestimate buffer → underrequest points → underrun
- Conservative (lower) estimate is safer: we might overfill slightly, but won't underrun

### Memory Layout

```rust
pub struct Stream {
    config: StreamConfig,
    caps: DacCapabilities,
    backend: Box<dyn StreamBackend>,

    // Pre-allocated buffers (no per-chunk allocation)
    chunk_buffer: Vec<LaserPoint>,      // Callback fills this
    last_chunk: Vec<LaserPoint>,        // For RepeatLast policy
    last_chunk_len: usize,

    // Timing state
    current_instant: StreamInstant,
    scheduled_ahead: u64,
    last_write_time: Instant,

    // Stats
    stats: StreamStats,
}
```

### Buffer Sizing

**Rule: Allocate to `max_points_per_chunk` from `DacCapabilities`.**

```rust
// At stream creation
let buffer_size = caps.max_points_per_chunk;
let chunk_buffer = vec![LaserPoint::default(); buffer_size];
let last_chunk = vec![LaserPoint::default(); buffer_size];
```

**Rationale:**
- Never exceeds what the DAC can accept in one write
- Large enough for any catch-up scenario (even if buffer is empty)
- Memory cost is negligible

**Per-DAC buffer sizes:**

| DAC | max_points_per_chunk | Buffer Memory (×2) |
|-----|---------------------|-------------------|
| Ether Dream | 1799 | ~58 KB |
| Helios | 4096 | ~130 KB |
| IDN | 4096 | ~130 KB |
| LaserCube USB | 4096 | ~130 KB |
| LaserCube WiFi | 6000 | ~192 KB |

Two buffers (`chunk_buffer` + `last_chunk`) total 60-200 KB depending on DAC. This is allocated once at stream start and reused for the stream's lifetime.

---

## Impact on Existing Components

### FrameAdapter

The existing `FrameAdapter` provides frame-level abstraction for applications that work with complete frames rather than time-based point generation.

**Current:**
```rust
pub fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint>
```

**Proposed:**
```rust
pub fn fill_chunk(&mut self, req: &FillRequest, buffer: &mut [LaserPoint]) -> FillResult
```

The adapter logic stays the same - it cycles through frame points and handles mid-chunk frame swaps. Only the interface changes to match the new zero-alloc design.

### Example Usage

#### Time-based (direct callback)

```rust
let config = StreamConfig {
    pps: 30_000,
    tick_interval: Duration::from_millis(10),
    target_buffer: Duration::from_millis(40),
    min_buffer: Duration::from_millis(10),
    underrun_policy: UnderrunPolicy::RepeatLast,
};

dac.stream(config, |req, buffer| {
    // target_points is already clamped to buffer.len() by the library
    let n = req.target_points;

    for i in 0..n {
        // req.start is estimated playback time; use for audio sync
        let t = req.start.as_secs_f64(req.pps) + (i as f64 / req.pps as f64);
        buffer[i] = generate_point_at_time(t);
    }

    FillResult::Filled(n)
})?;
```

#### Frame-based (using FrameAdapter)

```rust
// Single-threaded usage
let mut adapter = FrameAdapter::new();
adapter.update(circle_frame);

dac.stream(config, |req, buffer| {
    adapter.fill_chunk(req, buffer)
})?;
```

#### Frame-based with cross-thread updates (SharedFrameAdapter)

```rust
// Thread-safe usage with Arc<Mutex<_>>
let adapter = Arc::new(Mutex::new(FrameAdapter::new()));
adapter.lock().unwrap().update(circle_frame);

let adapter_clone = Arc::clone(&adapter);
dac.stream(config, move |req, buffer| {
    adapter_clone.lock().unwrap().fill_chunk(req, buffer)
})?;

// From another thread - safe because of Arc<Mutex<_>>
adapter.lock().unwrap().update(new_frame);
```

**Note:** The existing `SharedFrameAdapter` wrapper already provides this pattern. It will be updated to use the new `fill_chunk()` signature internally.

#### Audio-synced

```rust
dac.stream(config, |req, buffer| {
    let n = req.target_points;

    for i in 0..n {
        // req.start is estimated playback time, suitable for audio sync
        let t = req.start.as_secs_f64(req.pps) + (i as f64 / req.pps as f64);
        let audio = audio_analyzer.features_at(t);
        buffer[i] = visualize_audio(audio);
    }

    FillResult::Filled(n)
})?;
```

**Audio sync note:** `req.start` is the estimated playback time (playhead + buffered). For most audio-reactive applications, this provides sufficient accuracy. The `req.buffered` duration tells you the lookahead, which can be used to fetch audio features ahead of time.

---

## Migration Path

### Context: Clean Break

This crate is in **alpha with zero users**. Now is the time to be idealistic and get the API right before going public. There is no need for backwards compatibility, deprecation warnings, or gradual migration.

### Approach: Single PR Rewrite

This will be a **single PR that completely rewrites the streaming API**.

### No Backwards Compatibility

- No `_v2` suffixes
- No `#[deprecated]` attributes
- No feature flags for old vs new API
- No migration guides for existing users (there are none)

The goal is to ship the **ideal API** from day one of public release.

---

## Implementation Tasks

### Task 1: Core Types (`src/types.rs`)

- [ ] **1.1 Update `StreamInstant`**
  - [ ] Add `as_secs_f64(pps: u32) -> f64` method
  - [ ] Update documentation to clarify it represents estimated playback time
  - [ ] Test: `StreamInstant::as_secs_f64()` conversion accuracy

- [ ] **1.2 Create `FillRequest` struct**
  - [ ] Define struct with fields: `start`, `pps`, `min_points`, `target_points`, `buffered_points`, `buffered`, `device_queued_points`
  - [ ] Remove `max_points` (use `buffer.len()` instead)
  - [ ] Document rounding rules (ceiling for `min_points`)
  - [ ] Test: `FillRequest` construction and field access

- [ ] **1.3 Create `FillResult` enum**
  - [ ] Define variants: `Filled(usize)`, `Starved`, `End`
  - [ ] Document semantics for each variant
  - [ ] Document `Filled(0)` behavior (OK if `target_points == 0`, else treated as `Starved`)
  - [ ] Test: `FillResult` variant matching

- [ ] **1.4 Remove `ChunkRequest`**
  - [ ] Delete old struct
  - [ ] Update all imports and usages

- [ ] **1.5 Update `StreamConfig`**
  - [ ] Add `tick_interval: Duration` (default 10ms)
  - [ ] Add `target_buffer: Duration` (default 40ms)
  - [ ] Add `min_buffer: Duration` (default 10ms)
  - [ ] Keep existing `pps` and `underrun_policy`
  - [ ] Add builder methods for new fields
  - [ ] Test: `StreamConfig` defaults and builder pattern

---

### Task 2: Stream Internals (`src/stream.rs`)

- [ ] **2.1 Update callback signature**
  - [ ] Change from `FnMut(ChunkRequest) -> Option<Vec<LaserPoint>>`
  - [ ] To `FnMut(&FillRequest, &mut [LaserPoint]) -> FillResult`
  - [ ] Update all trait bounds and type aliases

- [ ] **2.2 Add pre-allocated buffers to `Stream`**
  - [ ] Add `chunk_buffer: Vec<LaserPoint>` (sized to `caps.max_points_per_chunk`)
  - [ ] Add `last_chunk: Vec<LaserPoint>` (sized to `caps.max_points_per_chunk`)
  - [ ] Add `last_chunk_len: usize`
  - [ ] Allocate buffers at stream creation
  - [ ] Test: Buffer allocation at correct size per DAC type

- [ ] **2.3 Implement fixed-tick timing loop**
  - [ ] Use `sleep_until(next_tick)` instead of `sleep(duration)`
  - [ ] Handle tick overrun (skip missed ticks, don't run back-to-back)
  - [ ] Implement early wake when buffer critically low (`buffered < min_buffer / 2`)
  - [ ] Test: Timing loop doesn't drift over many iterations
  - [ ] Test: Tick overrun handling skips correctly

- [ ] **2.4 Implement buffer state calculation**
  - [ ] Calculate `buffered_points` using conservative estimate
  - [ ] Calculate `buffered` Duration from points
  - [ ] Calculate `playhead` for `StreamInstant`
  - [ ] Calculate `min_points` with ceiling
  - [ ] Calculate `target_points` with ceiling, clamped to buffer length
  - [ ] Test: Point calculations with various buffer states

- [ ] **2.5 Implement conservative buffer estimation**
  - [ ] Use `min(hardware, software)` not `max`
  - [ ] Software estimate: `scheduled_ahead - (elapsed * pps)`
  - [ ] Trust hardware when available, but take minimum
  - [ ] Test: Estimation is conservative (never overestimates)

- [ ] **2.6 Implement `FillResult` handling**
  - [ ] `Filled(n)`: write to DAC, copy to `last_chunk`, update state
  - [ ] `Filled(0)` with `target_points > 0`: treat as `Starved`
  - [ ] `Starved`: apply underrun policy using `last_chunk` or blank
  - [ ] `Starved` with no `last_chunk`: fall back to blank
  - [ ] `End`: stop callback, drain queued points, park laser, return `RunExit::ProducerEnded`
  - [ ] Test: Each `FillResult` variant handled correctly
  - [ ] Test: `End` drains buffer before shutdown

- [ ] **2.7 Update `Stream::write()` method**
  - [ ] Accept slice reference instead of owned Vec
  - [ ] Remove point count validation (callback controls this now)
  - [ ] Update `last_chunk` via `copy_from_slice` (no allocation)
  - [ ] Test: Write with various point counts

---

### Task 3: Session Updates (`src/session.rs`)

- [ ] **3.1 Update session callback signature**
  - [ ] Match new `FnMut(&FillRequest, &mut [LaserPoint]) -> FillResult`
  - [ ] Update `run()` method signature
  - [ ] Test: Session runs with new callback signature

---

### Task 4: FrameAdapter Updates (`src/frame_adapter.rs`)

- [ ] **4.1 Update `FrameAdapter::fill_chunk()` method**
  - [ ] Rename from `next_chunk()` to `fill_chunk()`
  - [ ] Change signature to `(&mut self, req: &FillRequest, buffer: &mut [LaserPoint]) -> FillResult`
  - [ ] Fill provided buffer instead of allocating Vec
  - [ ] Use `req.target_points` instead of `req.n_points`
  - [ ] Return `FillResult::Filled(n)` instead of `Vec`
  - [ ] Test: Frame cycling works with new signature
  - [ ] Test: Mid-chunk frame swaps work correctly

- [ ] **4.2 Update `SharedFrameAdapter`**
  - [ ] Update `fill_chunk()` to match new signature
  - [ ] Ensure thread-safety with `Arc<Mutex<_>>`
  - [ ] Test: Cross-thread frame updates work correctly

- [ ] **4.3 Update FrameAdapter tests**
  - [ ] Convert all tests to use `FillRequest` and `FillResult`
  - [ ] Add test for `FillResult::Starved` when frame is empty
  - [ ] Add test for buffer filling behavior

---

### Task 5: Example Updates (`examples/`)

- [ ] **5.1 Update `callback.rs` example**
  - [ ] Use new callback signature
  - [ ] Demonstrate `FillRequest` fields usage
  - [ ] Show `target_points` usage

- [ ] **5.2 Update `manual.rs` example**
  - [ ] Use new `FillRequest` type
  - [ ] Update buffer handling

- [ ] **5.3 Update other examples**
  - [ ] Audit all examples for old API usage
  - [ ] Update to new patterns
  - [ ] Ensure all examples compile and run

- [ ] **5.4 Add audio-sync example (optional)**
  - [ ] Demonstrate `req.start.as_secs_f64(pps)` for timing
  - [ ] Show `req.buffered` usage for lookahead

---

### Task 6: Test Updates

- [ ] **6.1 Update existing stream tests**
  - [ ] Convert to new callback signature
  - [ ] Update assertions for new types

- [ ] **6.2 Add timing loop tests**
  - [ ] Test fixed-tick behavior
  - [ ] Test tick overrun handling
  - [ ] Test early wake behavior

- [ ] **6.3 Add buffer estimation tests**
  - [ ] Test conservative estimation (uses min)
  - [ ] Test with/without hardware queue reporting

- [ ] **6.4 Add `FillResult` handling tests**
  - [ ] Test `Filled(n)` normal path
  - [ ] Test `Filled(0)` edge cases
  - [ ] Test `Starved` with and without `last_chunk`
  - [ ] Test `End` graceful shutdown

- [ ] **6.5 Add integration tests**
  - [ ] Test full stream lifecycle with mock backend
  - [ ] Test underrun recovery
  - [ ] Test buffer refill after critical low

---

### Task 7: Documentation

- [ ] **7.1 Update module-level docs**
  - [ ] `src/stream.rs` - document new timing model
  - [ ] `src/types.rs` - document `FillRequest`, `FillResult`
  - [ ] `src/frame_adapter.rs` - document new API

- [ ] **7.2 Update README examples**
  - [ ] Show new callback signature
  - [ ] Document `StreamConfig` options

- [ ] **7.3 Add migration notes (internal)**
  - [ ] Document breaking changes for future reference

---

### Task 8: Final Verification

- [ ] **8.1 Run all tests**
  - [ ] `cargo test` passes
  - [ ] No warnings

- [ ] **8.2 Run all examples**
  - [ ] Each example compiles and runs
  - [ ] Verify with at least one real DAC if available

- [ ] **8.3 Run clippy**
  - [ ] `cargo clippy` passes
  - [ ] Address any new warnings

- [ ] **8.4 Verify documentation**
  - [ ] `cargo doc` builds without warnings
  - [ ] Review generated docs for accuracy

---

### Files Expected to Change

| File | Changes |
|------|---------|
| `src/types.rs` | `ChunkRequest` → `FillRequest`, add `FillResult`, update `StreamConfig` |
| `src/stream.rs` | Core timing loop rewrite, buffer management, callback signature |
| `src/frame_adapter.rs` | `next_chunk()` → `fill_chunk()` |
| `src/session.rs` | Update callback signature |
| `examples/*.rs` | All examples updated |
| `src/stream.rs` (tests) | Update inline tests |
| `src/frame_adapter.rs` (tests) | Update inline tests |

---

## Edge Cases and Constraints

### Edge Cases to Handle

| Edge Case | Behavior |
|-----------|----------|
| `Filled(0)` when `target_points == 0` | OK, buffer is full, nothing to do |
| `Filled(0)` when `target_points > 0` | Treat as `Starved`, apply underrun policy |
| `Starved` when `RepeatLast` has no last chunk | Fall back to `Blank` (blanked points at last position) |
| `End` while buffer has queued points | Let queued points drain before shutdown |
| Tick overrun (callback > tick_interval) | Skip missed ticks, don't run back-to-back |
| Hardware/software estimate divergence | Use `min` (conservative) to prevent underrun |
| `min_points > buffer.len()` | Clamp to `buffer.len()`, but this indicates config issue |

### DAC-Specific Constraints

| Constraint | Handling |
|------------|----------|
| Max points per write (e.g., Helios 4096) | `buffer.len()` is capped by `caps.max_points_per_chunk` |
| Binary ready/not-ready (Helios USB) | Can't do fine-grained buffer estimation; may need host-side queue or faster polling |
| Blocking write calls | Backend must return `WouldBlock` instead of blocking; stream handles retry |
| Network jitter (IDN, LaserCube WiFi) | Software estimate accounts for typical jitter; target_buffer provides headroom |

### Host-Side Queue for Binary-Ready DACs

For DACs like Helios USB that only report "ready/not ready":

```rust
// Option 1: Poll faster than tick_interval
// If ready, we can send; if not, wait

// Option 2: Build host-side queue
// Stream maintains a small buffer, drains to DAC when ready
// Allows fixed-tick callback even with binary DAC status
```

This is an implementation detail; the callback API remains the same.

---

## External Clock Mode (Future Enhancement)

For sample-accurate audio sync, consider a `Stream::tick()` API where an external clock (audio callback) drives timing:

```rust
// Audio callback drives timing
fn audio_callback(audio_buffer: &[f32]) {
    // Process audio...

    // Drive laser stream at audio's cadence
    let req = laser_stream.tick(); // Returns FillRequest
    let points = generate_from_audio(audio_buffer, &req);
    laser_stream.submit(&req, &points)?;
}
```

This is **not part of the initial rewrite** but the API should not preclude adding it later.

---

## Summary of Changes

| Aspect | Current | Proposed |
|--------|---------|----------|
| Timing | Variable (wait when full) | Fixed tick (~10ms), `sleep_until` semantics |
| Chunk size | Fixed (`n_points`) | Variable (`min_points`, `target_points`, `buffer.len()` cap) |
| Buffer | Callback allocates Vec | Library-owned slice, zero allocations |
| Return | `Option<Vec<_>>` | `FillResult` enum (`Filled`/`Starved`/`End`) |
| Buffer info | `scheduled_ahead_points` only | Both `buffered_points` and `buffered: Duration` |
| StreamInstant | Points since start | Estimated playback time (playhead + buffered) |
| PPS | Fixed per stream | Fixed per stream (unchanged) |
| Drift correction | None | Automatic via variable chunk size |
| Buffer estimation | N/A | Conservative (`min` of hardware/software) |

---

## Review Questions: Resolved

Based on feedback, the following questions have been resolved:

| Question | Resolution |
|----------|------------|
| Three tiers (`min`/`target`/`max`)? | **Two tiers**: `min` + `target`. Drop `max` from request; `buffer.len()` is the implicit cap. |
| `buffered` format? | **Both**: Include `buffered_points: u64` and `buffered: Duration` to avoid conversion overhead. |
| `FillResult` vs `Option<usize>`? | **Keep `FillResult`**: `Starved` vs `End` distinction is valuable for underrun handling. |
| External clock mode? | **Future enhancement**: Design should not preclude `Stream::tick()` API, but not part of initial rewrite. |
| DAC-specific constraints? | **Documented**: Binary ready/not-ready DACs may need host-side queue or faster polling. |
| Edge cases? | **Documented**: See Edge Cases section above. |
| `FrameAdapter` integration? | **Keep separate**: Consider making it implement a `Producer` trait for composability. |

## Open Questions for Implementation

1. **Producer trait**: Should we define a `trait Producer { fn fill(&mut self, req: &FillRequest, buffer: &mut [LaserPoint]) -> FillResult; }` that `FrameAdapter` implements?

2. **Host-side queue for Helios USB**: Should the initial implementation include this, or defer to a follow-up PR?

3. **Stats/diagnostics**: What metrics should `StreamStats` track? (missed ticks, underruns, buffer min/max, etc.)

4. **Graceful drain timeout**: How long to wait for queued points to drain on `End` before forcing shutdown?
