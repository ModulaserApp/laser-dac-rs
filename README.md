# laser-dac

Unified DAC backend abstraction for laser projectors.

This crate provides a complete solution for communicating with various laser DAC hardware:

- **Discovery**: Automatically find connected DAC devices (USB and network)
- **Workers**: Background threads for non-blocking frame output
- **Backends**: Unified `DacBackend` trait for all DAC types

This crate does not apply any additional processing on points (like blanking), except to make it compatible with the target DAC.

⚠️ **Warning**: use at your own risk! Laser projectors can be dangerous.

## Supported DACs

| DAC                       | Connection                     | Features | Verified
| ------------------------- | ------------------------------ | -------- | --------
| Helios                    | USB                            | 12-bit   | ✅
| Ether Dream               | Network                        | 16-bit   | ❌
| IDN                       | Network (ILDA Digital Network) | 16-bit   | ❌
| LaserCube WiFi            | WiFi                           | 16-bit   | ❌
| LaserCube USB / Laserdock | USB                            | 12-bit   | ❌

The DACs that are not verified, I have not tested with the DAC itself yet. Help to test these would be very welcome!

## Quick Start

Connect your Laser DAC and run an example:

```bash
cargo run --example high_level -- circle
# or:
cargo run --example high_level -- triangle
```

The examples run continuously until you press Ctrl+C.

## Discovery Behavior

There are two discovery APIs:

- `DacDiscoveryWorker` continuously scans in a background thread and auto-connects to new devices, yielding ready-to-use `DacWorker` instances.
- `DacDiscovery` is manual: you call `scan()` and decide if/when to `connect()` each `DiscoveredDevice`.

`DacDiscoveryWorker` runs a background thread that:

1. **Scans every 2 seconds** for new devices across all enabled DAC types
2. **Tracks known devices** to avoid duplicate connections
3. **Automatic reconnection** - after you call `mark_disconnected`, the device is removed from tracking and will reconnect on the next scan

Multiple devices of the same type are supported - each is identified by a unique name (MAC address for Ether Dream, serial number for LaserCube, etc.)

You can also provide a device filter predicate to allowlist devices before the worker connects:

```rust
use laser_dac::{DacDiscoveryWorker, DacType, EnabledDacTypes};

let discovery = DacDiscoveryWorker::new_with_device_filter(
    EnabledDacTypes::all(),
    |device| device.dac_type() == DacType::EtherDream,
);

// Update the predicate later while the worker is running.
discovery.set_device_filter(|device| {
    device.dac_type() == DacType::EtherDream && device.name().starts_with("ED-")
});
```

## Coordinate System

All backends use normalized coordinates:

- **X**: -1.0 (left) to 1.0 (right)
- **Y**: -1.0 (bottom) to 1.0 (top)
- **Colors**: 0-65535 for R, G, B, and intensity

Each backend handles conversion to its native format internally.

## Data Types

| Type                 | Description                                     |
| -------------------- | ----------------------------------------------- |
| `LaserFrame`         | Collection of points + PPS rate                 |
| `LaserPoint`         | Single point with position (f32) and color (u16) |
| `DacType`            | Enum of supported DAC hardware                  |
| `DacDevice`          | Device name + type                              |
| `DacConnectionState` | Connected or Lost state                         |

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
laser-dac = { version = "0.3", default-features = false, features = ["network-dacs"] }
```

### Other Features

| Feature | Description                                                    |
| ------- | -------------------------------------------------------------- |
| `serde` | Enable serde serialization for `DacType` and `EnabledDacTypes` |

### USB DAC Requirements

USB DACs (`helios`, `lasercube-usb`) use [rusb](https://crates.io/crates/rusb) which requires CMake to build.

# Acknowledgements

* Helios DAC: heavily inspired from [helios-dac](https://github.com/maxjoehnk/helios-dac-rs)
* Ether Dream DAC: heavily inspired from [ether-dream](https://github.com/nannou-org/ether-dream)
* Lasercube USB / WIFI: inspired from [ildagen](https://github.com/Grix/ildagen) (ported from C++ to Rust)
* IDN: inspired from [helios_dac](https://github.com/Grix/helios_dac) (ported from C++ to Rust)

## License

MIT
