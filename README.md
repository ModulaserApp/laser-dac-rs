# laser-dac

[![Crates.io](https://img.shields.io/crates/v/laser-dac.svg)](https://crates.io/crates/laser-dac)
[![Documentation](https://docs.rs/laser-dac/badge.svg)](https://docs.rs/laser-dac)
[![CI](https://github.com/ModulaserApp/laser-dac-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/ModulaserApp/laser-dac-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.87-blue.svg)](https://blog.rust-lang.org/2025/05/15/Rust-1.87.0.html)

Unified DAC backend abstraction for laser projectors.

This crate provides a complete solution for communicating with various laser DAC hardware:

- **Discovery**: Automatically find connected DAC devices (USB and network)
- **Frame API**: Submit complete frames with automatic transition blanking and looping
- **Streaming API**: Zero-allocation callback API with buffer-driven timing (FIFO backends only)
- **Backends**: Unified interface across FIFO and frame-swap DAC types

Frame-swap DACs (Helios) use the Frame API exclusively. FIFO DACs (Ether Dream,
LaserCube, IDN) work with both Frame and Streaming APIs.

## Supported DACs

| DAC                        | Connection | Backend      | Verified | Notes                                                                                                  |
| -------------------------- | ---------- | ------------ | -------- | ------------------------------------------------------------------------------------------------------ |
| Helios                     | USB        | Frame-swap   | ✅       | Frame API only                                                                                         |
| Ether Dream                | Network    | FIFO         | ✅       |
| IDN (ILDA Digital Network) | Network    | FIFO         | ✅       | IDN is a standardized protocol. We tested with [HeliosPRO](https://bitlasers.com/heliospro-laser-dac/) |
| LaserCube WiFi             | Network    | FIFO         | ✅       | Recommend to not use through WiFi mode; use LAN only                                                   |
| LaserCube USB / Laserdock  | USB        | FIFO         | ✅       |
| AVB Audio Device           | Network    | FIFO         | ✅       | Uses CoreAudio (macOS), ASIO on Windows by default (disable the `asio` default feature to use WASAPI and skip the Steinberg SDK), ALSA (Linux). Tested with [LaserAnimation Sollinger](https://laseranimation.com). |

All DACs have been manually verified to work.

## Quick Start

Connect your laser DAC and run an example:

```bash
# Frame API (recommended)
cargo run --example frame -- circle            # animated shape orbiting center
cargo run --example frame -- triangle
cargo run --example transitions -- default     # transition blanking between shapes
cargo run --example transitions -- animated    # 3 shapes cycling with transitions

# Streaming API (advanced)
cargo run --example stream -- circle
cargo run --example reconnect -- circle        # auto-reconnect on disconnect

# Other
cargo run --example audio                      # audio-reactive (requires microphone)
cargo run --example avb_file -- circle         # write 6ch AVB-mapped WAV for validation
```

The examples run continuously until you press Ctrl+C.

## Frame API (Recommended)

Submit complete frames and let the library handle looping, transition blanking, and
transport-appropriate delivery. This is the recommended path for most applications.

```rust
use laser_dac::{list_devices, open_device, Frame, FrameSessionConfig, LaserPoint};

let devices = list_devices()?;
let device = open_device(&devices[0].id)?;

let config = FrameSessionConfig::new(30_000);
let (session, info) = device.start_frame_session(config)?;

session.control().arm()?;

// Submit a frame — it loops automatically
session.send_frame(Frame::new(vec![
    LaserPoint::new(-0.5, -0.5, 65535, 0, 0, 65535),
    LaserPoint::new( 0.5, -0.5, 0, 65535, 0, 65535),
    LaserPoint::new( 0.0,  0.5, 0, 0, 65535, 65535),
]));

// Update content at any time — submission is zero-copy latest-wins:
// if the engine hasn't consumed the previous frame yet, the new one
// replaces it with no buffering or memory growth.
loop {
    let frame = generate_next_frame();
    session.send_frame(Frame::new(frame));
    std::thread::sleep(std::time::Duration::from_millis(16)); // ~60fps
}
```

For FIFO DACs, switching happens only at frame boundaries: the current frame is
always finished before the next frame is chosen. Latest-wins applies until the
next seam/chunk/frame has been materialized for transmission; once composed, that
output is retried verbatim until the DAC accepts it.

For advanced output-space processing on that final presented sequence, install
an [`OutputFilter`](https://docs.rs/laser-dac/latest/laser_dac/trait.OutputFilter.html)
with `FrameSessionConfig::with_output_filter(...)`.

### Empty Frames

Submitting `Frame::new(vec![])` blanks the output (sends a single blanked point at origin).
This is useful for intentionally clearing the display.

### Transition Blanking

When frames change, the library automatically inserts blanked transition points between
the last point of the outgoing frame and the first point of the incoming frame. The
default transition uses a 3-phase blanking sequence: end dwell (~100µs at source),
quintic-eased transit (distance-scaled by L-infinity distance, 0-64 points), and
start dwell (~400µs at destination). Dwell durations are converted to point counts
based on PPS.

You can supply your own transition function for custom blanking strategies.
The callback returns a `TransitionPlan` describing what to do at each seam:

```rust
use laser_dac::{FrameSessionConfig, LaserPoint, TransitionPlan};

let config = FrameSessionConfig::new(30_000)
    .with_transition_fn(Box::new(|from: &LaserPoint, to: &LaserPoint| {
        // Custom blanking: 4 linearly interpolated blank points
        TransitionPlan::Transition(
            (0..4).map(|i| {
                let t = (i + 1) as f32 / 5.0;
                LaserPoint::blanked(
                    from.x + (to.x - from.x) * t,
                    from.y + (to.y - from.y) * t,
                )
            }).collect()
        )
    }));
```

`TransitionPlan` has two variants:

- **`Transition(points)`** — keep both seam endpoints, insert `points` between them.
  `Transition(vec![])` keeps both endpoints with nothing in between.
- **`Coalesce`** — the two seam endpoints are the same logical point; emit only one
  copy. Use this for closed shapes (circles) to avoid a duplicate point at the seam.

Self-loops (A→A) also run through the transition callback, so seam planning is
consistent regardless of whether the frame changed.

For perfect loops where the seam endpoints are the same logical point, return
`Coalesce` to emit that seam point only once and avoid a visible halt.

For frame-swap DACs, if the authored frame plus transition points would exceed
the hardware capacity (e.g. Helios 4095 points), the transition prefix is
automatically truncated. Authored frame content is never dropped.

Or disable transition blanking entirely:

```rust
use laser_dac::{FrameSessionConfig, LaserPoint, TransitionPlan};

let config = FrameSessionConfig::new(30_000)
    .with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint| {
        TransitionPlan::Transition(vec![])
    }));
```

### Final Output Filter

`FrameSession` can run an optional `OutputFilter` on the exact point slice that
is about to be written to hardware.

- FIFO backends call it with `PresentedSliceKind::FifoChunk`
- Frame-swap backends call it with `PresentedSliceKind::FrameSwapFrame`
- Frame-swap slices are marked `is_cyclic = true` because the hardware loops the
  submitted frame
- The filter runs after transition composition, startup/disarm blanking, and
  color delay
- `WouldBlock` retries reuse the already-filtered buffer verbatim
- Pre-first-frame FIFO keepalive blanks do not invoke the filter

```rust
use laser_dac::{
    FrameSessionConfig, LaserPoint, OutputFilter, OutputFilterContext, OutputResetReason,
};

struct SafetyFilter;

impl OutputFilter for SafetyFilter {
    fn reset(&mut self, _reason: OutputResetReason) {}

    fn filter(&mut self, points: &mut [LaserPoint], _ctx: &OutputFilterContext) {
        for point in points {
            if point.x.abs() > 1.0 || point.y.abs() > 1.0 {
                *point = LaserPoint::blanked(point.x.clamp(-1.0, 1.0), point.y.clamp(-1.0, 1.0));
            }
        }
    }
}

let config = FrameSessionConfig::new(30_000)
    .with_output_filter(Box::new(SafetyFilter));
```

### Frame Session Liveness

`FrameSession::metrics()` exposes a small read-only liveness handle for
downstream watchdogs.

- `connected()` reports whether the session currently has a connected backend
- `last_loop_activity()` reports the last scheduler-thread progress timestamp
- `last_write_success()` reports the last successful backend write timestamp
- watchdog cadence, stall thresholds, and emergency-disarm policy remain
  downstream-owned

```rust
use laser_dac::{FrameSessionConfig, FrameSessionMetrics};

let config = FrameSessionConfig::new(30_000);
let (session, _info) = device.start_frame_session(config)?;
let metrics: FrameSessionMetrics = session.metrics();

if let Some(last) = metrics.last_loop_activity() {
    println!("scheduler last progressed {:?} ago", last.elapsed());
}
```

### Reconnecting Frame Session

For automatic reconnection when the device disconnects:

```rust
use laser_dac::{open_device, Frame, FrameSessionConfig, LaserPoint, ReconnectConfig};
use std::time::Duration;

let device = open_device("my-device")?;
let config = FrameSessionConfig::new(30_000)
    .with_reconnect(
        ReconnectConfig::new()
            .max_retries(5)
            .backoff(Duration::from_secs(1))
            .on_disconnect(|err| eprintln!("Lost connection: {}", err))
            .on_reconnect(|info| println!("Reconnected to {}", info.name))
    );

let (session, _info) = device.start_frame_session(config)?;

session.control().arm()?;

session.send_frame(Frame::new(vec![
    LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
]));
```

## Streaming API (Advanced)

The streaming API uses buffer-driven timing: your callback is invoked when the
buffer needs filling. This provides automatic backpressure handling and zero
allocations in the hot path. Use this for custom timing, procedural generation,
or audio-reactive content.

```rust
use laser_dac::{list_devices, open_device, ChunkRequest, ChunkResult, LaserPoint, StreamConfig};

let devices = list_devices()?;
let device = open_device(&devices[0].id)?;

let config = StreamConfig::new(30_000);
let (stream, info) = device.start_stream(config)?;

stream.control().arm()?;

let exit = stream.run(
    // Producer callback - fills buffer with points
    |req: &ChunkRequest, buffer: &mut [LaserPoint]| {
        let n = req.target_points;
        for i in 0..n {
            buffer[i] = generate_point(i);
        }
        ChunkResult::Filled(n)
    },
    // Error callback
    |err| eprintln!("Stream error: {}", err),
)?;
```

Return `ChunkResult::Filled(n)` to continue, `ChunkResult::End` to stop gracefully.

## Coordinate System

All backends use normalized coordinates:

- **X**: -1.0 (left) to 1.0 (right)
- **Y**: -1.0 (bottom) to 1.0 (top)
- **Colors**: 0-65535 for R, G, B, and intensity

Each backend handles conversion to its native format internally.

## Data Types

| Type                  | Description                                             |
| --------------------- | ------------------------------------------------------- |
| `Frame`       | Immutable frame of laser points for frame-mode output   |
| `FrameSession`        | Active frame-mode session with automatic looping        |
| `FrameSessionConfig`  | Frame session settings (PPS, transition fn, color delay) |
| `OutputFilter`        | Hook for transforming the final presented output        |
| `OutputFilterContext` | Metadata for a presented FIFO chunk or frame-swap frame |
| `PresentedSliceKind`  | Whether the filter saw a FIFO chunk or frame-swap frame |
| `OutputResetReason`   | Continuity reset reason for stateful output filters     |
| `TransitionFn`        | Callback for computing transition plan between frames   |
| `TransitionPlan`      | Enum: `Transition(points)` or `Coalesce` at seams       |
| `DacInfo`             | DAC metadata (name, type, capabilities)                 |
| `Dac`                 | Opened DAC ready for streaming                          |
| `Stream`              | Active streaming session (callback mode)                |
| `ReconnectConfig`     | Configuration for automatic reconnection                |
| `StreamConfig`        | Stream settings (PPS, buffering, color delay, blanking) |
| `ChunkRequest`        | Request info for filling point buffer                   |
| `LaserPoint`          | Single point with position (f32) and color (u16)        |
| `DacType`             | Enum of supported DAC hardware                          |

## Advanced Configuration

### Color Delay (Scanner Sync Compensation)

Galvo mirrors need time to settle before the laser fires. Color delay shifts
RGB+intensity channels relative to XY coordinates so colors arrive after the
mirrors are in position.

```rust
use std::time::Duration;

// Frame mode: set via config
let config = FrameSessionConfig::new(30_000)
    .with_color_delay_points(5);

// Streaming mode: set via config (applied at runtime)
let config = StreamConfig::new(30_000)
    .with_color_delay(Duration::from_micros(100));
```

Frame mode enables color delay by default (150us equivalent at the configured PPS).
It is applied statefully to the emitted point stream:

- FIFO DACs carry delay across chunks
- Frame-swap DACs carry delay across submitted frames
- Transition points between frames are included in the delayed stream

Streaming mode disables color delay by default. It can also be changed at runtime
via `stream.control().set_color_delay(...)`.

Typical values: 50-200us depending on scanner speed.

### Startup Blanking

Prevents the "flash on start" artifact by forcing the first points after arming
to blank, giving mirrors time to reach their initial position.

```rust
use std::time::Duration;

let config = FrameSessionConfig::new(30_000)
    .with_startup_blank(Duration::from_millis(2)); // default: 1ms
```

Set to `Duration::ZERO` to disable.

## Features

By default, all DAC protocols are enabled via the `all-dacs` feature.

### DAC Features

| Feature          | Description                                                 |
| ---------------- | ----------------------------------------------------------- |
| `all-dacs`       | Enable all DAC protocols (default)                          |
| `usb-dacs`       | Enable USB DACs: `helios`, `lasercube-usb`                  |
| `network-dacs`   | Enable network DACs: `ether-dream`, `idn`, `lasercube-wifi` |
| `audio-dacs`     | Enable audio DACs: `avb`                                    |
| `helios`         | Helios USB DAC                                              |
| `lasercube-usb`  | LaserCube USB (LaserDock) DAC                               |
| `ether-dream`    | Ether Dream network DAC                                     |
| `idn`            | ILDA Digital Network DAC                                    |
| `lasercube-wifi` | LaserCube WiFi DAC                                          |
| `avb`            | AVB audio-device backend (experimental)                     |

For example, to enable only network DACs:

```toml
[dependencies]
laser-dac = { version = "*", default-features = false, features = ["network-dacs"] }
```

### Other Features

| Feature | Description                                                    |
| ------- | -------------------------------------------------------------- |
| `serde` | Enable serde serialization for `DacType` and `EnabledDacTypes` |

### USB DAC Requirements

USB DACs (`helios`, `lasercube-usb`) use [rusb](https://crates.io/crates/rusb) which requires CMake to build.

## Development Tools

### IDN Simulator

The repository includes a debug simulator (in `tools/idn-simulator/`) that acts as a virtual IDN laser DAC. This is useful for testing and development without physical hardware.

```bash
# Build and run the simulator
cargo run -p idn-simulator

# With custom options
cargo run -p idn-simulator -- --hostname "Test-DAC" --service-name "Simulator" --port 7255
```

**Features:**

- Responds to IDN discovery (appears as a real DAC)
- Renders received laser frames as connected lines
- Handles blanking (intensity=0 creates gaps between shapes)
- Shows frame statistics (frame count, point count, client address)

**Usage:**

When the simulator is running, launch your work that scans for IDN devices. You can use this crate, or any other tool that supports IDN!

For a simple test, you can run one of our examples: `cargo run --example stream -- circle`

**CLI Options:**
| Option | Description | Default |
|--------|-------------|---------|
| `-n, --hostname` | Hostname in scan responses | `IDN-Simulator` |
| `-s, --service-name` | Service name in service map | `Simulator Laser` |
| `-p, --port` | UDP port to listen on | `7255` |

## Acknowledgements

- Helios DAC: heavily inspired from [helios-dac](https://github.com/maxjoehnk/helios-dac-rs)
- Ether Dream DAC: heavily inspired from [ether-dream](https://github.com/nannou-org/ether-dream)
- Lasercube USB / WIFI: inspired from [ildagen](https://github.com/Grix/ildagen) (ported from C++ to Rust)
- IDN: inspired from [helios_dac](https://github.com/Grix/helios_dac) (ported from C++ to Rust)

## License

MIT
