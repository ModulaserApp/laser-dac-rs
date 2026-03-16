# Modulaser Migration Guide — laser-dac frame-first architecture

This guide covers migrating Modulaser-v2 to the frame-first architecture changes in `laser-dac-rs` on the `frame-first-architecture` branch.

## Summary of breaking changes

| What changed | Old API | New API |
|---|---|---|
| Backend trait | `StreamBackend` (single trait) | `DacBackend` + `FifoBackend` or `FrameSwapBackend` |
| `try_write_chunk()` | method on `StreamBackend` | `try_write_points()` on `FifoBackend`, `write_frame()` on `FrameSwapBackend` |
| `ExternalDiscoverer::connect()` | returns `Box<dyn StreamBackend>` | returns `Box<dyn FifoBackend>` |
| `Dac::new()` | takes `Box<dyn StreamBackend>` | takes `BackendKind` |
| `Frame`, `FrameAdapter`, `SharedFrameAdapter` | public types | **removed** |
| `StreamBackend` re-export | `pub use StreamBackend` | gone — use `DacBackend`, `FifoBackend`, `FrameSwapBackend` |

## What does NOT change

- **`ReconnectingSession`** — same API, same behavior
- **`SessionControl`** — arm/disarm/stop/color_delay unchanged
- **`StreamConfig`** — unchanged
- **`ChunkRequest` / `ChunkResult`** — unchanged
- **`LaserPoint`** — unchanged
- **`DacDiscovery`** — `scan()`, `register()`, `open_by_id()` all unchanged
- **`list_devices()` / `open_device()`** — unchanged
- **Callback streaming** — `Stream::run()` works exactly as before

Modulaser's `connection.rs` uses `ReconnectingSession` with callback streaming. **This code needs zero changes.**

## Required changes

### 1. ShowNET backend: `StreamBackend` → `DacBackend` + `FrameSwapBackend`

ShowNET uses `OutputModel::UsbFrameSwap` and has frame-replacement semantics (each `WriteFrame` replaces the current scan). It should implement `FrameSwapBackend`.

**`src/dac/shownet/backend.rs`:**

```rust
// Old import:
use laser_dac::{
    DacCapabilities, DacType, LaserPoint, OutputModel, Result, StreamBackend, WriteOutcome,
};

// New import:
use laser_dac::{
    DacCapabilities, DacType, LaserPoint, OutputModel, Result, WriteOutcome,
    DacBackend, FrameSwapBackend,
};
```

Split `impl StreamBackend for ShowNetBackend` into two impl blocks:

```rust
impl DacBackend for ShowNetBackend {
    fn dac_type(&self) -> DacType { /* unchanged */ }
    fn caps(&self) -> &DacCapabilities { /* unchanged */ }
    fn connect(&mut self) -> Result<()> { /* unchanged */ }
    fn disconnect(&mut self) -> Result<()> { /* unchanged */ }
    fn is_connected(&self) -> bool { /* unchanged */ }
    fn stop(&mut self) -> Result<()> { /* unchanged */ }
    fn set_shutter(&mut self, open: bool) -> Result<()> { /* unchanged */ }
}

impl FrameSwapBackend for ShowNetBackend {
    fn frame_capacity(&self) -> usize {
        self.caps.max_points_per_chunk // 4096
    }

    fn is_ready_for_frame(&mut self) -> bool {
        if !self.is_open {
            return false;
        }
        let ready = unsafe { ShowNet_WaitForLastFrame(&self.device_id, 0) };
        if !ready {
            // Bypass readiness gate after timeout to detect session expiry
            self.last_successful_write.elapsed().as_millis() >= NOT_READY_FORCE_WRITE_MS
        } else {
            true
        }
    }

    fn write_frame(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        // Move the body of the old try_write_chunk here.
        // Remove the WaitForLastFrame readiness check (now in is_ready_for_frame).
        // The rest (point conversion, WriteFrame FFI call, error handling) stays.
        // ...
    }
}
```

**Key design decision:** The readiness check (`WaitForLastFrame`) moves to `is_ready_for_frame()`. The timeout-based bypass logic stays there too. `write_frame()` can still return `WouldBlock` for race conditions, so the retry from the stream scheduler still works.

**Tests:** Rename `try_write_chunk` → `write_frame` in all test call sites. The `FrameSwapBackend` trait methods are called the same way via `BackendKind::try_write()` dispatch, so no behavioral changes.

### 2. ShowNET discovery: `ExternalDiscoverer::connect()` return type

`ExternalDiscoverer::connect()` now returns `Box<dyn FifoBackend>`, not `Box<dyn StreamBackend>`. But ShowNET is a `FrameSwapBackend`, not a `FifoBackend`.

**Two options:**

**Option A (recommended): Register ShowNET directly with `DacDiscovery`**

Instead of using `ExternalDiscoverer`, add a `register_frame_swap()` method or wrap at the discovery level. The simplest approach: have `ShowNetSource::connect()` return `Box<dyn FifoBackend>` by wrapping ShowNET in a thin FIFO adapter. But this defeats the purpose.

**Option B: Expand `ExternalDiscoverer` to support both backend kinds**

Change the trait to return `BackendKind`:

```rust
// In laser-dac-rs discovery.rs:
pub trait ExternalDiscoverer: Send {
    fn dac_type(&self) -> DacType;
    fn scan(&mut self) -> Vec<ExternalDevice>;
    fn connect(&mut self, opaque_data: Box<dyn Any + Send>) -> Result<BackendKind>;
}
```

This is a cleaner solution since it lets external discoverers provide either backend kind. **This requires a change in laser-dac-rs itself** (one line in the trait + wrapping at the call site in `DacDiscovery::connect()`).

**For now:** Choose Option B — update `ExternalDiscoverer` in laser-dac-rs to return `BackendKind`, then update `ShowNetSource`:

```rust
// src/dac/shownet/discovery.rs
use laser_dac::{BackendKind, DacType, ExternalDevice, ExternalDiscoverer, Result};

impl ExternalDiscoverer for ShowNetSource {
    // ...
    fn connect(&mut self, opaque_data: Box<dyn Any + Send>) -> Result<BackendKind> {
        let info = opaque_data
            .downcast::<ShowNetConnectionInfo>()
            .map_err(|_| laser_dac::Error::invalid_config("Invalid ShowNET connection info"))?;

        Ok(BackendKind::FrameSwap(Box::new(ShowNetBackend::new(
            info.device_id,
            info.device_name,
        ))))
    }
}
```

### 3. Import updates across Modulaser

Search for `use laser_dac::StreamBackend` and `use laser_dac::Frame` — these no longer exist.

| File | Old import | New import |
|---|---|---|
| `shownet/backend.rs` | `StreamBackend` | `DacBackend, FrameSwapBackend` |
| `shownet/discovery.rs` | `StreamBackend` | `BackendKind` |

No other Modulaser files import these types.

## Optional: migrate to frame-first API

Modulaser currently renders chunks on-demand via `ReconnectingSession::run()` with a callback that calls `render_chunk_from_state()`. This works and doesn't need to change.

However, the new frame-first API (`FrameSession` + `AuthoredFrame`) could simplify the pipeline for DACs where Modulaser already computes full frames (the `LaserFrame` → snapshot → cursor traversal → chunk rendering path). Benefits:

- **Automatic transition blanking** between frames (no manual inter-chunk transitions)
- **Correct frame-swap behavior** for Helios and ShowNET (full frames sent atomically)
- **Simpler reconnection** via `ReconnectingSession::run_frame_session()` with last-frame replay

This is a larger refactor and can be done incrementally after the breaking changes above are resolved.

### What frame-mode migration would look like

```rust
// Instead of:
session.run(
    |req, buffer| render_chunk_from_state(&stream_state, req, buffer),
    |err| log::error!("Stream error: {}", err),
)?;

// Frame mode:
let handle = session.run_frame_session(FrameSessionConfig::new(pps))?;
// Then from the pipeline thread:
handle.send_frame(AuthoredFrame::new(frame_points));
```

The pipeline would submit `AuthoredFrame`s whenever the content changes, and the `FrameSession` handles cycling, transitions, and pacing internally.

## New types available (for reference)

| Type | Purpose |
|---|---|
| `DacBackend` | Common trait: connect, disconnect, shutter, stop |
| `FifoBackend` | Queue DACs: `try_write_points()`, `queued_points()` |
| `FrameSwapBackend` | Frame DACs: `write_frame()`, `is_ready_for_frame()`, `frame_capacity()` |
| `BackendKind` | Enum wrapping either backend kind |
| `AuthoredFrame` | Immutable frame (Arc-backed, cheap clone) |
| `FrameSession` | Frame-mode session with scheduler thread |
| `FrameSessionConfig` | Config: pps, transition_fn, startup_blank, color_delay_points |
| `TransitionFn` | Callback generating blanking points between frames |
| `default_transition()` | 8-point linear blanking transition |
| `FrameSessionHandle` | Submit frames to a reconnecting frame session |

## Migration order

1. **Update laser-dac-rs dependency** to the `frame-first-architecture` branch
2. **(In laser-dac-rs)** If going with Option B: change `ExternalDiscoverer::connect()` to return `BackendKind`
3. **Split ShowNetBackend** into `DacBackend` + `FrameSwapBackend` impls
4. **Update ShowNetSource** discovery return type
5. **Fix imports** — `StreamBackend` → new trait names
6. **Run tests** — Modulaser's ShowNET mock tests should pass with renamed methods
7. **(Optional, later)** Migrate pipeline to frame-mode API
