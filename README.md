# laser-dac

[![Crates.io](https://img.shields.io/crates/v/laser-dac.svg)](https://crates.io/crates/laser-dac)
[![Documentation](https://docs.rs/laser-dac/badge.svg)](https://docs.rs/laser-dac)
[![CI](https://github.com/ModulaserApp/laser-dac-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/ModulaserApp/laser-dac-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.87-blue.svg)](https://blog.rust-lang.org/2025/05/15/Rust-1.87.0.html)

Unified DAC backend abstraction for laser projectors.

This crate provides a complete solution for communicating with various laser DAC hardware:

- **Discovery**: Automatically find connected DAC devices (USB and network)
- **Streaming**: Zero-allocation callback API with buffer-driven timing
- **Backends**: Unified interface for all DAC types

This crate does not apply any additional processing on points (like blanking), except to make it compatible with the target DAC.

⚠️ **Warning**: use at your own risk! Laser projectors can be dangerous.

## Supported DACs

| DAC                        | Connection | Verified | Notes                                                                                                  |
| -------------------------- | ---------- | -------- | ------------------------------------------------------------------------------------------------------ |
| Helios                     | USB        | ✅       |
| Ether Dream                | Network    | ✅       |
| IDN (ILDA Digital Network) | Network    | ✅       | IDN is a standardized protocol. We tested with [HeliosPRO](https://bitlasers.com/heliospro-laser-dac/) |
| LaserCube WiFi             | Network    | ✅       | Recommend to not use through WiFi mode; use LAN only                                                       |
| LaserCube USB / Laserdock  | USB        | ✅       |
| AVB Audio Device           | Network    | ⚠️       | Experimental; audio output backend using CoreAudio (macOS), ASIO (Windows), ALSA (Linux)                    |

All DACs have been manually verified to work.

## Quick Start

Connect your laser DAC and run an example. For full API details, see the [documentation](https://docs.rs/laser-dac).

```bash
cargo run --example stream -- circle
cargo run --example stream -- triangle
cargo run --example callback -- circle      # with progress counter
cargo run --example frame_adapter -- circle # using FrameAdapter
cargo run --example reconnect -- circle     # auto-reconnect on disconnect
cargo run --example audio                   # audio-reactive (requires microphone)
cargo run --example avb_file -- circle      # write 6ch AVB-mapped WAV for validation
```

The examples run continuously until you press Ctrl+C.

## Streaming API

The streaming API uses buffer-driven timing: your callback is invoked when the
buffer needs filling. This provides automatic backpressure handling and zero
allocations in the hot path.

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

### Reconnecting Session (optional)

If you want automatic reconnection by device ID, use `ReconnectingSession`:

```rust
use laser_dac::{ChunkRequest, ChunkResult, LaserPoint, ReconnectingSession, StreamConfig};
use std::time::Duration;

let mut session = ReconnectingSession::new("my-device", StreamConfig::new(30_000))
    .with_max_retries(5)
    .with_backoff(Duration::from_secs(1))
    .on_disconnect(|err| eprintln!("Lost connection: {}", err))
    .on_reconnect(|info| println!("Reconnected to {}", info.name));

// Arm output as usual (this persists across reconnects)
session.control().arm()?;

let exit = session.run(
    |req: &ChunkRequest, buffer: &mut [LaserPoint]| {
        let n = req.target_points;
        for i in 0..n {
            buffer[i] = generate_point(i);
        }
        ChunkResult::Filled(n)
    },
    |err| eprintln!("Stream error: {}", err),
)?;
```

Note: `ReconnectingSession` uses `open_device()` internally, so it won't include
external discoverers registered on a custom `DacDiscovery`.

## Coordinate System

All backends use normalized coordinates:

- **X**: -1.0 (left) to 1.0 (right)
- **Y**: -1.0 (bottom) to 1.0 (top)
- **Colors**: 0-65535 for R, G, B, and intensity

Each backend handles conversion to its native format internally.

## Data Types

| Type           | Description                                      |
| -------------- | ------------------------------------------------ |
| `DacInfo`      | DAC metadata (name, type, capabilities)          |
| `Dac`          | Opened DAC ready for streaming                   |
| `Stream`       | Active streaming session                         |
| `ReconnectingSession` | Stream wrapper with automatic reconnect   |
| `StreamConfig` | Stream settings (PPS, buffering, color delay, blanking) |
| `ChunkRequest`  | Request info for filling point buffer            |
| `LaserPoint`   | Single point with position (f32) and color (u16) |
| `DacType`      | Enum of supported DAC hardware                   |

## Advanced Configuration

### Color Delay (Scanner Sync Compensation)

Galvo mirrors need time to settle before the laser fires. `color_delay` shifts
RGB+intensity channels relative to XY coordinates so colors arrive after the
mirrors are in position.

```rust
use std::time::Duration;

let config = StreamConfig::new(30_000)
    .with_color_delay(Duration::from_micros(100));
```

Can also be changed at runtime via `stream.control().set_color_delay(...)`.

Typical values: 50–200µs depending on scanner speed. Disabled by default.

### Startup Blanking

Prevents the "flash on start" artifact by forcing the first points after arming
to blank, giving mirrors time to reach their initial position.

```rust
use std::time::Duration;

let config = StreamConfig::new(30_000)
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
