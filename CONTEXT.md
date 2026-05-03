# Project Context

This document defines load-bearing terms used in `laser-dac-rs`. Names matter: the same idea should not appear under multiple names, and unrelated ideas should not collide. Update this file when a term is sharpened during design or review.

## Domain terms

### DAC
A physical (or virtual) laser projector endpoint that consumes a stream of [Points](#point) and drives galvos/lasers. Examples: Helios, Ether Dream, IDN-capable projector, LaserCube, AVB audio device, oscilloscope in XY mode.

A `DacType` value identifies the *kind* of DAC; an individual DAC instance is a [DiscoveredDevice](#discovereddevice) once located on the system or network.

### Protocol
The wire format and discovery mechanism a particular [DAC](#dac) uses. Each protocol lives in its own module under `src/protocols/<name>/` and is feature-gated. Examples: `helios`, `ether_dream`, `idn`, `lasercube_usb`, `lasercube_wifi`, `avb`, `oscilloscope`.

A protocol owns its wire encoding, its [Backend](#backend), and its [Discoverer](#discoverer).

### Point
A single laser sample: normalized `(x, y)` plus 16-bit RGB and intensity. The crate's neutral type is `LaserPoint` in `src/types.rs`. Each [Protocol](#protocol) converts `LaserPoint` to its own wire format inside its own module — `LaserPoint` itself does not know about wire formats.

### Frame
An ordered sequence of [Points](#point) representing one logical image. `Frame` (in `src/presentation/`) wraps an `Arc<Vec<LaserPoint>>` and is cheap to clone.

### Backend
The runtime object that holds a connection to a single [DAC](#dac) and accepts writes. A backend implements `DacBackend` plus exactly one of `FifoBackend` (stream-of-points model) or `FrameSwapBackend` (whole-frame ping-pong model).

`BackendKind` is the type-erased enum that hides which sub-trait a particular backend implements.

### Discoverer
A protocol-owned object that locates [DACs](#dac) of one [DacType](#dac) on the local system or network and produces [DiscoveredDevices](#discovereddevice) for them.

A `Discoverer` is responsible for:
- enumerating reachable DACs of its type (`scan`),
- formatting [stable_id](#stable_id) values for them,
- accepting opaque per-device data back at connect time and producing a [Backend](#backend).

The `Discoverer` trait is the seam between the discovery registry and any individual protocol. Built-in protocols and external (downstream) protocols both implement the same trait — there is no second-class "external discoverer."

### DiscoveredDevice
A located but not-yet-connected [DAC](#dac). Carries:
- a [DiscoveredDeviceInfo](#discovereddeviceinfo) (cloneable, hashable identity record),
- the [Backend](#backend) capabilities (`DacCapabilities`) snapshotted at scan time,
- opaque per-protocol connect data that the producing [Discoverer](#discoverer) accepts back.

### DiscoveredDeviceInfo
A small, cloneable, hashable identity record for a [DiscoveredDevice](#discovereddevice). Carries the [DacType](#dac), a human-readable `name`, an immutable [stable_id](#stable_id), and protocol-agnostic identifier hints (IP, MAC, hostname, USB address) that the [Discoverer](#discoverer) populates at scan time.

`DiscoveredDeviceInfo` is the right type to filter, log, or display devices without holding the connection-bearing `DiscoveredDevice`.

### stable_id
A canonical, namespaced string identifier for a [DiscoveredDevice](#discovereddevice), formatted by the producing [Discoverer](#discoverer) at scan time. The id is stable across IP changes, USB re-enumerations, and process restarts where possible (e.g. EtherDream uses MAC; IDN uses mDNS hostname; Helios uses USB serial).

`stable_id` is the canonical key for tracking, deduplication, and `open_device(id)` lookup. Each protocol prefixes its ids with its own protocol slug (`etherdream:`, `idn:`, `helios:`, …) by convention.

## Architectural terms

These come from the `improve-codebase-architecture` workflow. Use them when discussing structural changes:

- **Module** — anything with an interface and an implementation.
- **Interface** — everything a caller must know to use a module: types, invariants, error modes, ordering, config.
- **Seam** — where an interface lives; a place behaviour can be altered without editing in place.
- **Adapter** — a concrete implementation of an interface at a seam.
- **Deep / shallow** — depth describes leverage at the interface. Deep = a lot of behaviour behind a small interface. Shallow = the interface is nearly as complex as the implementation.
