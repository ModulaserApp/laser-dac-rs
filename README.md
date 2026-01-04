# laser-dac

[![Crates.io](https://img.shields.io/crates/v/laser-dac.svg)](https://crates.io/crates/laser-dac)
[![Documentation](https://docs.rs/laser-dac/badge.svg)](https://docs.rs/laser-dac)
[![CI](https://github.com/SpaceK33z/laser-dac-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/SpaceK33z/laser-dac-rs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.87-blue.svg)](https://blog.rust-lang.org/2025/05/15/Rust-1.87.0.html)

Unified DAC backend abstraction for laser projectors.

This crate provides a complete solution for communicating with various laser DAC hardware:

- **Discovery**: Automatically find connected DAC devices (USB and network)
- **Workers**: Background threads for non-blocking frame output
- **Backends**: Unified `DacBackend` trait for all DAC types

This crate does not apply any additional processing on points (like blanking), except to make it compatible with the target DAC.

⚠️ **Warning**: use at your own risk! Laser projectors can be dangerous.

## Supported DACs

| DAC                        | Connection | Verified |
| -------------------------- | ---------- | -------- |
| Helios                     | USB        | ✅       |
| Ether Dream                | Network    | ✅       |
| IDN (ILDA Digital Network) | Network    | ✅       |
| LaserCube WiFi             | Network    | ❌       |
| LaserCube USB / Laserdock  | USB        | ✅       |

The DACs that are not verified, I have not tested with the DAC itself yet. Help to test these would be very welcome!

IDN a standardized protocol. So any DAC that supports IDN would be supported. The implementation was tested with the [HeliosPRO](https://bitlasers.com/heliospro-laser-dac/).

## Quick Start

Connect your laser DAC and run an example. For full API details, see the [documentation](https://docs.rs/laser-dac).

```bash
cargo run --example automatic -- circle
# or:
cargo run --example automatic -- triangle
# callback mode (DAC-driven timing):
cargo run --example callback -- circle
```

The examples run continuously until you press Ctrl+C.

## Discovery Behavior

There are two discovery APIs:

- `DacDiscoveryWorker` continuously scans in a background thread and auto-connects to new devices (predicate optional), yielding ready-to-use `DacWorker` instances.
- `DacDiscovery` is manual: you call `scan()` and decide if/when to `connect()` each `DiscoveredDevice`.

`DacDiscoveryWorker` runs a background thread that:

1. **Scans periodically** (default: every 2 seconds, configurable via `discovery_interval()`) for new devices across all enabled DAC types
2. **Reports all discovered devices** via `poll_discovered_devices()` regardless of filter
3. **Auto-connects to filtered devices** and yields ready-to-use workers via `poll_new_workers()`
4. **Automatic reconnection** - when a device connection fails, it's removed from tracking and will reconnect on the next scan

Multiple devices of the same type are supported - each is identified by a unique name (MAC address for Ether Dream, serial number for LaserCube, etc.)

You can provide a device filter predicate to control which devices are auto-connected:

```rust
use laser_dac::{DacDiscoveryWorker, DacType, EnabledDacTypes};
use std::time::Duration;

let discovery = DacDiscoveryWorker::builder()
    .enabled_types(EnabledDacTypes::all())
    .device_filter(|info| info.dac_type == DacType::EtherDream)
    .discovery_interval(Duration::from_secs(1))
    .build();

// All discovered devices are visible, even those not matching the filter
for device in discovery.poll_discovered_devices() {
    println!("Found: {} ({:?})", device.name(), device.dac_type);
}

// Only filtered devices become workers
for worker in discovery.poll_new_workers() {
    println!("Connected: {}", worker.device_name());
}
```

## Streaming Modes

There are two ways to stream frames to a DAC:

### Push Mode (DacWorker)

You control timing by calling `submit_frame()` in your main loop. Simple and works well for most cases:

```rust
use laser_dac::{DacDiscoveryWorker, EnabledDacTypes, LaserFrame};

let discovery = DacDiscoveryWorker::builder()
    .enabled_types(EnabledDacTypes::all())
    .build();

// Get workers from discovery...
let mut workers: Vec<_> = discovery.poll_new_workers().collect();

loop {
    let frame = create_your_frame();
    for worker in &mut workers {
        worker.update();
        worker.submit_frame(frame.clone());
    }
    std::thread::sleep(std::time::Duration::from_millis(33));
}
```

### Callback Mode (DacCallbackWorker)

The DAC drives timing by invoking your callback whenever it's ready for more data. This is ideal when you want the DAC to control the frame rate, or when generating frames on-demand:

```rust
use laser_dac::{DacCallbackWorker, CallbackError, LaserFrame};
use laser_dac::discovery::DacDiscovery;

let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
for device in discovery.scan() {
    let backend = discovery.connect(device.clone())?;
    let mut worker = DacCallbackWorker::new(
        device.name().to_string(),
        device.dac_type(),
        backend,
    );

    worker.start(
        // Data callback - invoked when device needs more data
        |ctx| {
            let frame = generate_frame(ctx.frames_written);
            Some(frame)
        },
        // Error callback - invoked on connection loss
        |err: CallbackError| {
            eprintln!("Error: {:?}", err);
        },
    );
}
```

The callback receives a context with `frames_written` count. Return `Some(frame)` to send data, or `None` to signal completion.

## Coordinate System

All backends use normalized coordinates:

- **X**: -1.0 (left) to 1.0 (right)
- **Y**: -1.0 (bottom) to 1.0 (top)
- **Colors**: 0-65535 for R, G, B, and intensity

Each backend handles conversion to its native format internally.

## Data Types

| Type                 | Description                                      |
| -------------------- | ------------------------------------------------ |
| `LaserFrame`         | Collection of points + PPS rate                  |
| `LaserPoint`         | Single point with position (f32) and color (u16) |
| `DacType`            | Enum of supported DAC hardware                   |
| `DacDevice`          | Device name + type                               |
| `DacConnectionState` | Connected or Lost state                          |

## Features

By default, all DAC protocols are enabled via the `all-dacs` feature.

### DAC Features

| Feature          | Description                                                 |
| ---------------- | ----------------------------------------------------------- |
| `all-dacs`       | Enable all DAC protocols (default)                          |
| `usb-dacs`       | Enable USB DACs: `helios`, `lasercube-usb`                  |
| `network-dacs`   | Enable network DACs: `ether-dream`, `idn`, `lasercube-wifi` |
| `helios`         | Helios USB DAC                                              |
| `lasercube-usb`  | LaserCube USB (LaserDock) DAC                               |
| `ether-dream`    | Ether Dream network DAC                                     |
| `idn`            | ILDA Digital Network DAC                                    |
| `lasercube-wifi` | LaserCube WiFi DAC                                          |

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

## Acknowledgements

- Helios DAC: heavily inspired from [helios-dac](https://github.com/maxjoehnk/helios-dac-rs)
- Ether Dream DAC: heavily inspired from [ether-dream](https://github.com/nannou-org/ether-dream)
- Lasercube USB / WIFI: inspired from [ildagen](https://github.com/Grix/ildagen) (ported from C++ to Rust)
- IDN: inspired from [helios_dac](https://github.com/Grix/helios_dac) (ported from C++ to Rust)

## License

MIT
