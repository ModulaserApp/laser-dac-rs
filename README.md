# laser-dac

Unified DAC backend abstraction for laser projectors.

This crate provides a complete solution for communicating with various laser DAC hardware:

- **Discovery**: Automatically find connected DAC devices (USB and network)
- **Workers**: Background threads for non-blocking frame output
- **Backends**: Unified `DacBackend` trait for all DAC types

This crate does not apply any additional processing on points (like blanking), except to make it compatible with the target DAC.

## Supported DACs

| DAC                       | Connection                     | Features |
| ------------------------- | ------------------------------ | -------- |
| Helios                    | USB                            | 12-bit   |
| Ether Dream               | Network                        | 16-bit   |
| IDN                       | Network (ILDA Digital Network) | 16-bit   |
| LaserCube WiFi            | WiFi                           | 16-bit   |
| LaserCube USB / Laserdock | USB                            | 12-bit   |

## Quick Start

The easiest way to use this crate is with `DacDiscoveryWorker`:

```rust
use laser_dac::{DacDiscoveryWorker, EnabledDacTypes, LaserFrame, LaserPoint};
use std::thread;
use std::time::Duration;

fn main() {
    // Start background discovery (finds all connected DACs)
    let discovery = DacDiscoveryWorker::new(EnabledDacTypes::all());

    // Collect workers as devices are found
    let mut workers = Vec::new();
    for _ in 0..50 {
        for worker in discovery.poll_new_workers() {
            println!("Found: {} ({})", worker.device_name(), worker.dac_type());
            workers.push(worker);
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Create a frame
    let frame = LaserFrame::new(30000, vec![
        LaserPoint::blanked(-0.5, -0.5),
        LaserPoint::new(-0.5, -0.5, 255, 0, 0, 255),
        LaserPoint::new(0.5, -0.5, 0, 255, 0, 255),
        LaserPoint::new(0.0, 0.5, 0, 0, 255, 255),
        LaserPoint::new(-0.5, -0.5, 255, 0, 0, 255),
    ]);

    // Send frames to all connected DACs
    for worker in &mut workers {
        worker.update();
        worker.submit_frame(frame.clone());
    }

    // Stop output when done
    for worker in &workers {
        worker.stop_output();
    }
}
```

## Low-Level API

For more control, use `UnifiedDiscovery` directly:

```rust
use laser_dac::{UnifiedDiscovery, DacWorker, EnabledDacTypes};

fn main() -> Result<(), String> {
    // Create discovery (initializes USB controllers)
    let mut discovery = UnifiedDiscovery::new(EnabledDacTypes::all());

    // Scan for devices
    let devices = discovery.scan();

    for device in devices {
        println!("Found: {} ({})", device.name(), device.dac_type());

        // Connect when ready
        let backend = discovery.connect(device)?;

        // Create worker from backend
        let worker = DacWorker::new(
            device.name().to_string(),
            device.dac_type(),
            backend,
        );

        // Use worker...
    }

    Ok(())
}
```

## API Overview

### High-Level (Recommended)

| Type                 | Description                                              |
| -------------------- | -------------------------------------------------------- |
| `DacDiscoveryWorker` | Background discovery that produces ready-to-use workers  |
| `DacWorker`          | Background frame writer with channel-based communication |
| `EnabledDacTypes`    | Configure which DAC types to discover                    |

### Mid-Level

| Type               | Description                           |
| ------------------ | ------------------------------------- |
| `UnifiedDiscovery` | Manual scan/connect for all DAC types |
| `DiscoveredDevice` | Device info returned by scan          |

### Low-Level

| Type                                       | Description                                    |
| ------------------------------------------ | ---------------------------------------------- |
| `DacBackend`                               | Trait for direct frame writing                 |
| `HeliosBackend`, `EtherDreamBackend`, etc. | Per-DAC backend implementations                |
| `WriteResult`                              | Result of write operation (Written/DeviceBusy) |

### Data Types

| Type                 | Description                                     |
| -------------------- | ----------------------------------------------- |
| `LaserFrame`         | Collection of points + PPS rate                 |
| `LaserPoint`         | Single point with position (f32) and color (u16) |
| `DacType`            | Enum of supported DAC hardware                  |
| `DacDevice`          | Device name + type                              |
| `DacConnectionState` | Connected or Lost state                         |

## Coordinate System

All backends use normalized coordinates:

- **X**: -1.0 (left) to 1.0 (right)
- **Y**: -1.0 (bottom) to 1.0 (top)
- **Colors**: 0-65535 for R, G, B, and intensity

Each backend handles conversion to its native format internally.

## Running Examples

```bash
# Run the unified example (discovers devices and sends test frames)
cargo run --example unified
```

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

### Other Features

| Feature | Description                                                    |
| ------- | -------------------------------------------------------------- |
| `serde` | Enable serde serialization for `DacType` and `EnabledDacTypes` |

### USB DAC Requirements

USB DACs (`helios`, `lasercube-usb`) use [rusb](https://crates.io/crates/rusb) which requires CMake to build.

### Examples

Enable only network DACs:

```toml
[dependencies]
laser-dac = { version = "0.1", default-features = false, features = ["network-dacs"] }
```

Enable only Helios:

```toml
[dependencies]
laser-dac = { version = "0.1", default-features = false, features = ["helios"] }
```

## License

MIT
