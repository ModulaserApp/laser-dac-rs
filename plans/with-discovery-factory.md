# Add `open_device_with()` and `Dac::with_discovery_factory()` for Custom Backend Reconnection

This plan adds a public way to open custom-backend DACs with a caller-provided `DacDiscovery`, and ensures the same discovery setup is reused during automatic reconnects. In practice, it introduces `open_device_with()` for the normal open path, `Dac::with_discovery_factory()` for manually constructed `Dac`s, updates reconnect error messages, and adds tests that prove custom devices can be rediscovered after disconnect.

## Context

Modulaser uses `ExternalDiscoverer` to register a custom ShowNET backend via `DacDiscovery::register()`. When a FrameSession or Stream reconnects after a disconnect, the internal `ReconnectTarget::open_device()` falls back to `crate::open_device()`, which creates a fresh `DacDiscovery::new(EnabledDacTypes::all())` — this default discovery does **not** include any externally registered backends. ShowNET (and any future custom backend) will never be found on reconnect.

The infrastructure for solving this already exists internally — `ReconnectTarget` has a `discovery_factory: Option<DiscoveryFactory>` field, and `open_device()` checks it before falling back. But there's no public API for consumers to set it.

**Two problems need solving:**
1. **Opening custom-backend devices**: `open_device()` can't find custom backends (no `register()` call). `DacDiscovery::open_by_id()` could, but it's `pub(crate)`.
2. **Reconnecting to custom-backend devices**: Even if the initial open succeeds, reconnection uses default discovery and will fail.

**Goal**: Add a public `open_device_with()` that solves both problems in one call, plus a `Dac::with_discovery_factory()` for consumers who build `Dac` via the `scan()` + `connect()` + `Dac::new()` path.

## Current State

### ReconnectTarget (src/reconnect.rs, line 28-43)

```rust
pub(crate) struct ReconnectTarget {
    pub device_id: String,
    pub discovery_factory: Option<DiscoveryFactory>,
}

impl ReconnectTarget {
    pub fn open_device(&self) -> Result<Dac> {
        if let Some(factory) = &self.discovery_factory {
            let mut discovery = factory();
            discovery.open_by_id(&self.device_id)
        } else {
            crate::open_device(&self.device_id)
        }
    }
}
```

The `DiscoveryFactory` type (same file, line 21):
```rust
pub(crate) type DiscoveryFactory = Box<dyn Fn() -> DacDiscovery + Send + 'static>;
```

### open_device() (src/lib.rs, line 214-222)

```rust
pub fn open_device(id: &str) -> BackendResult<Dac> {
    let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
    let mut dac = discovery.open_by_id(id)?;
    dac.reconnect_target = Some(reconnect::ReconnectTarget {
        device_id: id.to_string(),
        discovery_factory: None,  // <-- always None
    });
    Ok(dac)
}
```

### DacDiscovery::open_by_id() (src/discovery.rs, line 987-1016)

```rust
pub(crate) fn open_by_id(&mut self, id: &str) -> Result<Dac> {
    // Scans, finds device by stable_id, connects, returns Dac::new(info, backend)
    // Note: Dac::new() sets reconnect_target = None
}
```

This is `pub(crate)` — external consumers cannot call it.

### Dac struct (src/stream/mod.rs, line 1205-1209)

```rust
pub struct Dac {
    info: DacInfo,
    backend: Option<BackendKind>,
    pub(crate) reconnect_target: Option<ReconnectTarget>,
}
```

`reconnect_target` is `pub(crate)` — consumers cannot access it.

### How reconnect flows through to FrameSession

1. `Dac::start_frame_session(config)` (src/stream/mod.rs, line 1364) takes `self.reconnect_target`
2. If `config.reconnect` is `Some`, wraps target in `ReconnectPolicy::new(rc, target)` (line 1381)
3. `FrameSession::start()` passes policy into the scheduler thread (src/presentation/session.rs, line 128)
4. On disconnect, `FrameSession::try_reconnect()` calls `policy.target.open_device()` (via `reconnect_backend_with_retry` in src/reconnect.rs, line 138)
5. That calls `ReconnectTarget::open_device()` which uses the factory if present

Same flow for `Dac::start_stream()` (line 1285) → `Stream` scheduler.

### Error messages that become misleading (src/stream/mod.rs, lines 1317 and 1379)

Both `start_stream()` and `start_frame_session()` contain:
```rust
Error::invalid_config("reconnect requires a device opened via open_device()")
```

Once `with_discovery_factory()` exists, devices created via `Dac::new()` can also have a valid reconnect target. These error messages need updating.

---

## Implementation

### 1. Add `open_device_with()` — primary API for custom backends

**File**: `src/lib.rs`, after `open_device()` (around line 222)

This is the main entry point for consumers with custom backends. It solves both initial discovery and reconnection in one call.

```rust
/// Open a DAC by ID using a custom discovery factory.
///
/// Like [`open_device`], but uses the provided factory to create the
/// [`DacDiscovery`] instance. This is required for custom backends
/// registered via [`DacDiscovery::register`] — the default `open_device`
/// only finds built-in DAC types.
///
/// The factory is called once now for the initial open, and stored for
/// future reconnection attempts. It must be `Fn` (not `FnOnce`) because
/// reconnection may call it multiple times.
///
/// # Example
///
/// ```ignore
/// use laser_dac::{open_device_with, DacDiscovery, EnabledDacTypes, FrameSessionConfig, ReconnectConfig};
///
/// let dac = open_device_with("shownet:my-device", || {
///     let mut d = DacDiscovery::new(EnabledDacTypes::all());
///     d.register(Box::new(MyShowNetDiscoverer::new()));
///     d
/// })?;
///
/// let config = FrameSessionConfig::new(30_000)
///     .with_reconnect(ReconnectConfig::new());
/// let (session, _info) = dac.start_frame_session(config)?;
/// // Reconnection will also use the factory to find the custom backend
/// ```
pub fn open_device_with<F>(id: &str, factory: F) -> BackendResult<Dac>
where
    F: Fn() -> DacDiscovery + Send + 'static,
{
    let mut discovery = factory();
    let mut dac = discovery.open_by_id(id)?;
    dac.reconnect_target = Some(reconnect::ReconnectTarget {
        device_id: id.to_string(),
        discovery_factory: Some(Box::new(factory)),
    });
    Ok(dac)
}
```

Also add to public re-exports in `src/lib.rs`:
```rust
// Currently only open_device is public, add open_device_with next to it
```

### 2. Add `Dac::with_discovery_factory()` — secondary API

**File**: `src/stream/mod.rs`, after `Dac::new()` (around line 1219)

For consumers who open devices through their own `scan()` + `connect()` + `Dac::new()` path (e.g., Modulaser's discovery worker finds devices separately).

```rust
/// Set a custom discovery factory for reconnection.
///
/// When reconnection is enabled (via [`FrameSessionConfig::with_reconnect`] or
/// [`StreamConfig::with_reconnect`]), the factory is called to create a
/// [`DacDiscovery`] instance for each reconnection attempt. This is required
/// for custom backends registered via [`DacDiscovery::register`] — without it,
/// reconnection uses the default discovery which only finds built-in DAC types.
///
/// For most cases, prefer [`open_device_with`](crate::open_device_with) which
/// handles both initial discovery and reconnection in one call. Use this method
/// when you build `Dac` instances yourself via `scan()` + `connect()` + `Dac::new()`.
///
/// # Example
///
/// ```ignore
/// use laser_dac::{Dac, DacDiscovery, EnabledDacTypes, FrameSessionConfig, ReconnectConfig};
///
/// // Device opened through custom discovery path
/// let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
/// discovery.register(Box::new(MyCustomDiscoverer::new()));
/// let devices = discovery.scan();
/// let backend = discovery.connect(devices.into_iter().next().unwrap())?;
/// let dac = Dac::new(info, backend);
///
/// // Attach factory so reconnection can also find custom backends
/// let dac = dac.with_discovery_factory(|| {
///     let mut d = DacDiscovery::new(EnabledDacTypes::all());
///     d.register(Box::new(MyCustomDiscoverer::new()));
///     d
/// });
///
/// let config = FrameSessionConfig::new(30_000)
///     .with_reconnect(ReconnectConfig::new());
/// let (session, _info) = dac.start_frame_session(config)?;
/// ```
pub fn with_discovery_factory<F>(mut self, factory: F) -> Self
where
    F: Fn() -> DacDiscovery + Send + 'static,
{
    match self.reconnect_target {
        Some(ref mut target) => {
            target.discovery_factory = Some(Box::new(factory));
        }
        None => {
            self.reconnect_target = Some(ReconnectTarget {
                device_id: self.info.id.clone(),
                discovery_factory: Some(Box::new(factory)),
            });
        }
    }
    self
}
```

Note the `None` arm: `DacDiscovery::open_by_id()` returns `Dac::new()` which has `reconnect_target: None`. The method must handle both cases.

### 3. Update error messages in start_stream / start_frame_session

**File**: `src/stream/mod.rs`, lines 1317 and 1379

Replace:
```rust
Error::invalid_config("reconnect requires a device opened via open_device()")
```

With:
```rust
Error::invalid_config(
    "reconnect requires a reconnect target — use open_device(), \
     open_device_with(), or Dac::with_discovery_factory()"
)
```

### 4. No other changes needed

- `ReconnectTarget` stays `pub(crate)` — accessed internally by the new methods
- `DiscoveryFactory` type stays `pub(crate)` — already the right shape
- `DacDiscovery::open_by_id()` stays `pub(crate)` — called from `open_device_with()` which is in the same crate

---

## Tests

All test backends use existing fixtures from the test suite (`TestBackend`, `FifoTestBackend`, `FailingWriteBackend`, etc.). No `mock_backend()` helper — construct directly.

### Unit tests (required)

**File**: `src/stream/tests.rs`

```rust
#[test]
fn with_discovery_factory_creates_target_when_none() {
    let backend = TestBackend::new();
    let info = DacInfo::new("test-id", "Test", DacType::Custom("Test".into()), backend.caps().clone());
    let dac = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    assert!(dac.reconnect_target.is_none());

    let dac = dac.with_discovery_factory(|| DacDiscovery::new(EnabledDacTypes::all()));
    let target = dac.reconnect_target.as_ref().unwrap();
    assert_eq!(target.device_id, "test-id");
    assert!(target.discovery_factory.is_some());
}

#[test]
fn with_discovery_factory_replaces_factory_on_existing_target() {
    let backend = TestBackend::new();
    let info = DacInfo::new("my-id", "Test", DacType::Custom("Test".into()), backend.caps().clone());
    let mut dac = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    dac.reconnect_target = Some(crate::reconnect::ReconnectTarget {
        device_id: "my-id".to_string(),
        discovery_factory: None,
    });

    let dac = dac.with_discovery_factory(|| DacDiscovery::new(EnabledDacTypes::all()));
    let target = dac.reconnect_target.as_ref().unwrap();
    assert_eq!(target.device_id, "my-id");
    assert!(target.discovery_factory.is_some());
}

#[test]
fn start_session_with_discovery_factory_target_succeeds() {
    // Verify that with_discovery_factory creates a valid reconnect target
    // that start_frame_session accepts (no "requires open_device" error)
    let backend = TestBackend::new();
    let info = DacInfo::new("test", "Test", DacType::Custom("Test".into()), backend.caps().clone());
    let dac = Dac::new(info, BackendKind::Fifo(Box::new(backend)));
    let dac = dac.with_discovery_factory(|| DacDiscovery::new(EnabledDacTypes::all()));

    let config = FrameSessionConfig::new(30_000)
        .with_reconnect(ReconnectConfig::new().max_retries(0));
    let result = dac.start_frame_session(config);
    assert!(result.is_ok(), "with_discovery_factory target should be accepted: {:?}", result.err());
    let (session, _) = result.unwrap();
    let _ = session.control().stop();
}
```

### Reconnect integration test (required)

**File**: `src/stream/tests.rs`

This test lives in `src/stream/tests.rs` (not `presentation/tests.rs`) because it needs the `FailingWriteBackend` fixture defined there (line 1946). It also needs `FifoTestBackend` for the reconnected backend — either import from `presentation/tests.rs` (if made `pub(crate)` behind `#[cfg(test)]`) or create a minimal equivalent inline. Simplest: define a small `ReconnectFifoBackend` in this test that just accepts writes.

The test creates a mock `ExternalDiscoverer` that:
- Tracks `scan()` call count via `Arc<AtomicU32>`
- Returns a connectable `FifoTestBackend` device on `connect()`
- Uses a stable device ID matching the initial Dac

It then uses `FailingWriteBackend` (existing fixture, disconnects after N writes) as the initial backend, and asserts that after disconnect the `on_reconnect` callback fires — proving the factory-provided discoverer successfully found and reconnected the device.

**Stable ID matching is critical.** `open_by_id()` matches on `DiscoveredDeviceInfo::stable_id()`. For `DacType::Custom(name)` the stable ID format is (from `src/discovery.rs:309-317`):
- With `ip_address`: `"{name_lowercase}:{ip}"`
- With `hardware_name` (no IP): `"{name_lowercase}:{hardware_name}"`
- Neither: `"unknown:Custom(\"{name}\")"`

The initial `DacInfo.id` and the discoverer's produced stable ID **must be identical** or `open_by_id` will fail to find the device. The test uses `ip_address` for a deterministic stable ID: `"trackingtest:10.0.0.99"`.

```rust
/// Mock ExternalDiscoverer that tracks scan/connect calls and returns
/// a FifoTestBackend on connect. Uses a fixed IP so the stable_id is
/// deterministic: "trackingtest:10.0.0.99"
struct TrackingDiscoverer {
    scan_count: Arc<AtomicU32>,
    connect_count: Arc<AtomicU32>,
}

impl ExternalDiscoverer for TrackingDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Custom("TrackingTest".into())
    }

    fn scan(&mut self) -> Vec<ExternalDevice> {
        self.scan_count.fetch_add(1, Ordering::SeqCst);
        let mut device = ExternalDevice::new(());  // opaque_data = ()
        device.ip_address = Some("10.0.0.99".parse().unwrap());
        device.hardware_name = Some("Tracking Test Device".into());
        vec![device]
    }

    fn connect(&mut self, _opaque_data: Box<dyn Any + Send>) -> Result<BackendKind> {
        self.connect_count.fetch_add(1, Ordering::SeqCst);
        Ok(BackendKind::Fifo(Box::new(FifoTestBackend::new())))
    }
}

#[test]
fn reconnect_rediscovers_custom_backend_via_factory() {
    use std::sync::atomic::AtomicU32;

    let scan_count = Arc::new(AtomicU32::new(0));
    let connect_count = Arc::new(AtomicU32::new(0));
    let reconnected = Arc::new(AtomicBool::new(false));

    let scan_count_factory = scan_count.clone();
    let connect_count_factory = connect_count.clone();
    let reconnected_cb = reconnected.clone();

    // Initial backend: disconnects after 2 writes (use FailingWriteBackend pattern)
    let initial_backend = /* FailingWriteBackend-style backend that errors after N writes */ ;
    let caps = initial_backend.caps().clone();

    // stable_id for Custom("TrackingTest") with ip=10.0.0.99 is "trackingtest:10.0.0.99"
    // This MUST match what TrackingDiscoverer::scan() produces via stable_id()
    let device_id = "trackingtest:10.0.0.99";
    let info = DacInfo::new(
        device_id,
        "Tracking Test Device",
        DacType::Custom("TrackingTest".into()),
        caps,
    );

    let dac = Dac::new(info, BackendKind::Fifo(Box::new(initial_backend)));
    let dac = dac.with_discovery_factory(move || {
        let mut d = DacDiscovery::new(EnabledDacTypes::none());
        d.register(Box::new(TrackingDiscoverer {
            scan_count: scan_count_factory.clone(),
            connect_count: connect_count_factory.clone(),
        }));
        d
    });

    let config = FrameSessionConfig::new(30_000)
        .with_reconnect(
            ReconnectConfig::new()
                .max_retries(3)
                .backoff(Duration::from_millis(50))
                .on_reconnect(move |_info| {
                    reconnected_cb.store(true, Ordering::SeqCst);
                })
        );
    let (session, _info) = dac.start_frame_session(config).unwrap();
    session.control().arm().unwrap();
    session.send_frame(Frame::new(vec![LaserPoint::blanked(0.0, 0.0)]));

    // Wait for disconnect → reconnect cycle
    std::thread::sleep(Duration::from_millis(500));

    // Assert the full chain: factory called → discoverer scanned → device found by ID → connected → session resumed
    assert!(scan_count.load(Ordering::SeqCst) > 0,
        "discoverer scan() should have been called during reconnect");
    assert!(connect_count.load(Ordering::SeqCst) > 0,
        "discoverer connect() should have been called to reopen the device");
    assert!(reconnected.load(Ordering::SeqCst),
        "on_reconnect callback should have fired, proving successful reconnect");

    let _ = session.control().stop();
}
```

The three assertions prove the full reconnect chain:
1. `scan_count > 0` — factory produced a discovery that scanned the custom discoverer
2. `connect_count > 0` — `open_by_id("trackingtest:10.0.0.99")` matched the discoverer's device and called `connect()`
3. `reconnected == true` — `on_reconnect` callback fired, proving the session resumed with the new backend

This cannot pass if the stable ID doesn't match, if the discoverer isn't registered, or if reconnection fails at any stage.

---

## Consumer Usage (Modulaser)

### Primary path: `open_device_with()`

```rust
let dac = open_device_with(&device_id, move || {
    let mut d = DacDiscovery::new(enabled_types.clone());
    #[cfg(all(feature = "shownet", any(target_os = "macos", target_os = "windows")))]
    d.register(Box::new(ShowNetSource::new()));
    d
})?;

let config = FrameSessionConfig::new(stream_params.pps)
    .with_reconnect(ReconnectConfig::new().backoff(Duration::from_secs(1)));
let (session, info) = dac.start_frame_session(config)?;
```

### Alternative path: `scan()` + `connect()` + `with_discovery_factory()`

For Modulaser's discovery worker which already runs `DacDiscovery::scan()` in a background thread:

```rust
// Discovery worker already found the device
let backend = discovery.connect(discovered_device)?;
let dac = Dac::new(dac_info, backend);

// Attach factory for reconnection
let dac = dac.with_discovery_factory(move || {
    let mut d = DacDiscovery::new(enabled_types.clone());
    d.register(Box::new(ShowNetSource::new()));
    d
});

let config = FrameSessionConfig::new(stream_params.pps)
    .with_reconnect(ReconnectConfig::new().backoff(Duration::from_secs(1)));
let (session, info) = dac.start_frame_session(config)?;
```

---

## Verification

```bash
cargo check --all-targets && cargo test --quiet && cargo clippy --all-targets -- -D warnings
```

## Files Changed

| File | Change |
|------|--------|
| `src/lib.rs` | Add `open_device_with()` function (~25 lines) + re-export |
| `src/stream/mod.rs` | Add `Dac::with_discovery_factory()` method (~30 lines), update 2 error messages |
| `src/stream/tests.rs` | Add unit tests + reconnect integration test |

No breaking changes. This is a purely additive API.
