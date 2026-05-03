//! DAC device discovery.
//!
//! Defines the [`Discoverer`] trait — the seam where each protocol plugs in
//! its own scanning, identity formatting, and connect-time setup — plus the
//! [`DacDiscovery`] registry that aggregates them.

use std::any::Any;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;

use crate::backend::{BackendKind, Error, Result};
use crate::types::{caps_for_dac_type, DacCapabilities, DacType, EnabledDacTypes};

// =============================================================================
// Discoverer trait
// =============================================================================

/// A protocol-owned object that locates DACs of one [`DacType`] and produces
/// [`DiscoveredDevice`]s for them.
///
/// Built-in and external (downstream) protocols both implement the same trait.
///
/// # Example
///
/// ```ignore
/// use laser_dac::{
///     BackendKind, DacCapabilities, DacType, DiscoveredDevice,
///     DiscoveredDeviceInfo, Discoverer, Result,
/// };
/// use std::any::Any;
///
/// struct MyClosedDacDiscoverer { /* ... */ }
///
/// struct MyConnectData { /* ... */ }
///
/// impl Discoverer for MyClosedDacDiscoverer {
///     fn dac_type(&self) -> DacType {
///         DacType::Custom("MyClosedDAC".into())
///     }
///
///     fn prefix(&self) -> &str { "myclosed" }
///
///     fn scan(&mut self) -> Vec<DiscoveredDevice> {
///         // build DiscoveredDevice with stable_id and caps populated
///         vec![]
///     }
///
///     fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
///         todo!()
///     }
/// }
/// ```
pub trait Discoverer: Send {
    /// The DAC type this discoverer handles.
    fn dac_type(&self) -> DacType;

    /// Stable-id prefix this discoverer's devices use, e.g. `"etherdream"`.
    ///
    /// Used by `open_by_id` to narrow scans to a single discoverer when the
    /// caller's id has a known prefix. Must be unique across registered
    /// discoverers; [`DacDiscovery::register`] panics on duplicates.
    fn prefix(&self) -> &str;

    /// Locate reachable DACs on the system / network.
    ///
    /// Each returned [`DiscoveredDevice`] must have its `stable_id` and `caps`
    /// populated by the implementer.
    fn scan(&mut self) -> Vec<DiscoveredDevice>;

    /// Open a connection to a previously-scanned device.
    ///
    /// `opaque` is the connect-time data the discoverer attached to the
    /// `DiscoveredDevice`. Implementations downcast it to their internal type.
    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind>;
}

// =============================================================================
// DiscoveredDevice
// =============================================================================

/// A discovered but not-yet-connected DAC device.
///
/// Use [`DacDiscovery::connect`] to establish a connection and get a backend.
pub struct DiscoveredDevice {
    info: DiscoveredDeviceInfo,
    caps: DacCapabilities,
    discoverer_index: Option<usize>,
    connect_data: Box<dyn Any + Send>,
}

impl DiscoveredDevice {
    /// Build a device with capabilities derived from `info.dac_type` via
    /// [`caps_for_dac_type`]. Use [`with_caps`](Self::with_caps) when caps
    /// must be authored from runtime data (e.g., oscilloscope sample rate).
    pub fn new(info: DiscoveredDeviceInfo, connect_data: Box<dyn Any + Send>) -> Self {
        let caps = caps_for_dac_type(&info.dac_type);
        Self {
            info,
            caps,
            discoverer_index: None,
            connect_data,
        }
    }

    /// Override the auto-derived capabilities. Chainable from [`new`](Self::new).
    pub fn with_caps(mut self, caps: DacCapabilities) -> Self {
        self.caps = caps;
        self
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn dac_type(&self) -> &DacType {
        &self.info.dac_type
    }

    pub fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    pub fn info(&self) -> &DiscoveredDeviceInfo {
        &self.info
    }
}

impl fmt::Debug for DiscoveredDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiscoveredDevice")
            .field("info", &self.info)
            .field("caps", &self.caps)
            .field("discoverer_index", &self.discoverer_index)
            .field("connect_data", &"<opaque>")
            .finish()
    }
}

/// Lightweight info about a discovered device.
///
/// Cloneable and used for filtering, logging, and display without consuming
/// the original [`DiscoveredDevice`].
///
/// # Identity contract
///
/// [`PartialEq`], [`Eq`], and [`Hash`] consider only [`stable_id`](Self::stable_id);
/// the other fields (IP, MAC, hostname, …) are presentation hints that may
/// drift between scans and do not affect identity. This is sound for infos
/// produced by a registered [`Discoverer`] because [`DacDiscovery::register`]
/// guarantees [`Discoverer::prefix`] is unique, which makes the
/// `<prefix>:<rest>` namespace globally collision-free across discoverers.
///
/// If you construct a `DiscoveredDeviceInfo` manually (outside the registry)
/// you are responsible for keeping `stable_id` collision-free; otherwise two
/// distinct devices may compare equal.
#[derive(Debug, Clone)]
pub struct DiscoveredDeviceInfo {
    /// The DAC type.
    pub dac_type: DacType,
    /// Canonical, namespaced identifier set by the producing [`Discoverer`].
    pub stable_id: String,
    /// Human-readable name set by the producing [`Discoverer`].
    pub name: String,
    /// IP address for network devices.
    pub ip_address: Option<IpAddr>,
    /// MAC address (Ether Dream).
    pub mac_address: Option<[u8; 6]>,
    /// Hostname (IDN).
    pub hostname: Option<String>,
    /// USB bus:address (LaserCube USB).
    pub usb_address: Option<String>,
    /// Hardware/serial name (Helios, LaserCube USB, AVB).
    pub hardware_name: Option<String>,
    /// Disambiguation index when multiple identical devices are present (AVB).
    pub device_index: Option<u16>,
}

impl DiscoveredDeviceInfo {
    /// Build a minimal info with all identifier hints set to `None`.
    /// Use the `with_*` chainable setters to populate the protocol-relevant
    /// fields.
    pub fn new(dac_type: DacType, stable_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            dac_type,
            stable_id: stable_id.into(),
            name: name.into(),
            ip_address: None,
            mac_address: None,
            hostname: None,
            usb_address: None,
            hardware_name: None,
            device_index: None,
        }
    }

    pub fn with_ip(mut self, ip: IpAddr) -> Self {
        self.ip_address = Some(ip);
        self
    }

    pub fn with_mac(mut self, mac: [u8; 6]) -> Self {
        self.mac_address = Some(mac);
        self
    }

    pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    pub fn with_usb_address(mut self, addr: impl Into<String>) -> Self {
        self.usb_address = Some(addr.into());
        self
    }

    pub fn with_hardware_name(mut self, name: impl Into<String>) -> Self {
        self.hardware_name = Some(name.into());
        self
    }

    pub fn with_device_index(mut self, index: u16) -> Self {
        self.device_index = Some(index);
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The id is prefixed with the protocol slug (`etherdream:`, `idn:`,
    /// `helios:`, …) and is stable across IP/USB re-enumerations where the
    /// protocol can compute one.
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }
}

impl PartialEq for DiscoveredDeviceInfo {
    fn eq(&self, other: &Self) -> bool {
        self.stable_id == other.stable_id
    }
}

impl Eq for DiscoveredDeviceInfo {}

impl Hash for DiscoveredDeviceInfo {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.stable_id.hash(state);
    }
}

/// Downcast the connect-data box passed to [`Discoverer::connect`] back to
/// the producing protocol's concrete type.
///
/// A failure here indicates a registry invariant violation: a
/// `DiscoveredDevice` was routed to a discoverer that did not produce it.
/// This is unreachable in practice — [`DacDiscovery`] tags every scanned
/// device with the index of its producing discoverer and dispatches
/// `connect()` back to that same discoverer.
pub fn downcast_connect_data<T: 'static>(
    opaque: Box<dyn Any + Send>,
    protocol: &str,
) -> Result<Box<T>> {
    opaque.downcast::<T>().map_err(|_| {
        Error::invalid_config(format!(
            "internal: connect data for {} routed to wrong discoverer (registry invariant violation)",
            protocol
        ))
    })
}

/// Slug a free-form device name into an ASCII-safe id segment.
///
/// Used by protocols whose stable id derives from a human-readable device
/// name (AVB, Oscilloscope).
pub fn slugify_device_id(name: &str) -> String {
    let normalized = name
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_ascii_lowercase();
    let mut slug = String::with_capacity(normalized.len());
    let mut prev_dash = false;

    for ch in normalized.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }

    slug.trim_matches('-').to_string()
}

// =============================================================================
// DacDiscovery — registry
// =============================================================================

fn register_builtins(_this: &mut DacDiscovery, _enabled: &EnabledDacTypes) {
    #[cfg(feature = "helios")]
    if _enabled.is_enabled(DacType::Helios) {
        if let Some(d) = crate::protocols::helios::HeliosDiscoverer::new() {
            _this.register(Box::new(d));
        }
    }
    #[cfg(feature = "ether-dream")]
    if _enabled.is_enabled(DacType::EtherDream) {
        _this.register(Box::new(
            crate::protocols::ether_dream::EtherDreamDiscoverer::new(),
        ));
    }
    #[cfg(feature = "idn")]
    if _enabled.is_enabled(DacType::Idn) {
        _this.register(Box::new(crate::protocols::idn::IdnDiscoverer::new()));
    }
    #[cfg(feature = "lasercube-wifi")]
    if _enabled.is_enabled(DacType::LasercubeWifi) {
        _this.register(Box::new(
            crate::protocols::lasercube_wifi::LasercubeWifiDiscoverer::new(),
        ));
    }
    #[cfg(feature = "lasercube-usb")]
    if _enabled.is_enabled(DacType::LasercubeUsb) {
        if let Some(d) = crate::protocols::lasercube_usb::LasercubeUsbDiscoverer::new() {
            _this.register(Box::new(d));
        }
    }
    #[cfg(feature = "oscilloscope")]
    if _enabled.is_enabled(DacType::Oscilloscope) {
        _this.register(Box::new(
            crate::protocols::oscilloscope::OscilloscopeDiscoverer::new(),
        ));
    }
    #[cfg(feature = "avb")]
    if _enabled.is_enabled(DacType::Avb) {
        _this.register(Box::new(crate::protocols::avb::AvbDiscoverer::new()));
    }
}

/// DAC discovery coordinator.
///
/// Owns a registry of [`Discoverer`] implementations — both built-in and
/// downstream-registered — and dispatches `scan`/`connect` across them.
///
/// Built-in discoverers are registered eagerly by [`DacDiscovery::new`] for
/// each [`DacType`] enabled in the supplied [`EnabledDacTypes`]. To replace
/// a built-in with a custom-configured one (e.g., IDN with specific scan
/// addresses for testing), construct with that type masked out and then
/// [`register`](Self::register) the custom discoverer.
pub struct DacDiscovery {
    discoverers: Vec<Box<dyn Discoverer>>,
}

impl DacDiscovery {
    /// Create a new DAC discovery instance.
    ///
    /// Eagerly registers all built-in discoverers whose [`DacType`] is in
    /// `enabled` and whose feature flag is on. USB-backed discoverers
    /// (Helios, LaserCube USB) are silently skipped if their controller
    /// fails to initialize.
    pub fn new(enabled: EnabledDacTypes) -> Self {
        let mut this = Self {
            discoverers: Vec::new(),
        };
        register_builtins(&mut this, &enabled);
        this
    }

    /// Register a discoverer.
    ///
    /// # Panics
    ///
    /// Panics if [`Discoverer::prefix`] is empty, or if another registered
    /// discoverer already uses the same prefix. Prefixes must be unique and
    /// non-empty to keep `open_by_id` dispatch deterministic — an empty
    /// prefix would otherwise capture every prefix-less id.
    pub fn register(&mut self, discoverer: Box<dyn Discoverer>) {
        let prefix = discoverer.prefix().to_string();
        assert!(
            !prefix.is_empty(),
            "DacDiscovery::register: discoverer prefix must not be empty"
        );
        if self.discoverers.iter().any(|d| d.prefix() == prefix) {
            panic!(
                "DacDiscovery::register: duplicate discoverer prefix {:?}",
                prefix
            );
        }
        self.discoverers.push(discoverer);
    }

    /// Scan for available DAC devices across all registered discoverers.
    pub fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let mut out = Vec::new();
        for idx in 0..self.discoverers.len() {
            out.extend(self.scan_one(idx));
        }
        out
    }

    fn scan_one(&mut self, idx: usize) -> Vec<DiscoveredDevice> {
        let mut devices = self.discoverers[idx].scan();
        for device in &mut devices {
            device.discoverer_index = Some(idx);
        }
        devices
    }

    /// Connect to a previously discovered device.
    pub fn connect(&mut self, device: DiscoveredDevice) -> Result<BackendKind> {
        let idx = device.discoverer_index.ok_or_else(|| {
            Error::invalid_config(
                "DiscoveredDevice has no discoverer_index — was it produced by a registry scan?",
            )
        })?;
        let stable_id = &device.info.stable_id;
        let discoverer = self.discoverers.get_mut(idx).ok_or_else(|| {
            Error::invalid_config(format!(
                "discoverer for {} not found in registry",
                stable_id
            ))
        })?;
        discoverer.connect(device.connect_data)
    }

    /// Scan for a device by stable ID, connect, and return a `Dac`.
    ///
    /// When the ID carries a protocol prefix (`prefix:rest`), only that
    /// protocol's discoverer is scanned. If the prefix matches no registered
    /// discoverer, the call fails fast without scanning. A prefix-less id
    /// falls back to scanning every registered discoverer.
    pub(crate) fn open_by_id(&mut self, id: &str) -> Result<crate::stream::Dac> {
        let (id_prefix, has_prefix) = match id.split_once(':') {
            Some((p, _)) => (p, true),
            None => ("", false),
        };
        let matching = self
            .discoverers
            .iter()
            .position(|d| d.prefix() == id_prefix);
        let discovered = match (matching, has_prefix) {
            (Some(idx), _) => self.scan_one(idx),
            (None, true) => {
                return Err(Error::disconnected(format!(
                    "DAC not found: {} (no discoverer registered for prefix {:?})",
                    id, id_prefix
                )));
            }
            (None, false) => self.scan(),
        };

        let device = discovered
            .into_iter()
            .find(|d| d.info.stable_id == id)
            .ok_or_else(|| Error::disconnected(format!("DAC not found: {}", id)))?;

        let name = device.info.name.clone();
        let dac_type = device.info.dac_type.clone();
        let stream_backend = self.connect(device)?;

        let dac_info = crate::types::DacInfo {
            id: id.to_string(),
            name,
            kind: dac_type,
            caps: stream_backend.caps().clone(),
        };

        Ok(crate::stream::Dac::new(dac_info, stream_backend))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::backend::{BackendKind, DacBackend, FifoBackend};
    use crate::types::{DacCapabilities, LaserPoint};
    use crate::WriteOutcome;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    // -------------------------------------------------------------------------
    // Mock discoverer for registry tests + canonical example for downstream.
    // -------------------------------------------------------------------------

    #[derive(Debug, Clone)]
    struct MockConnectionInfo {
        _device_id: u32,
    }

    struct MockBackend {
        connected: bool,
    }

    impl DacBackend for MockBackend {
        fn dac_type(&self) -> DacType {
            DacType::Custom("MockDAC".into())
        }

        fn caps(&self) -> &DacCapabilities {
            static CAPS: DacCapabilities = DacCapabilities {
                pps_min: 1,
                pps_max: 100_000,
                max_points_per_chunk: 4096,
                output_model: crate::types::OutputModel::NetworkFifo,
            };
            &CAPS
        }

        fn connect(&mut self) -> Result<()> {
            self.connected = true;
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            Ok(())
        }
    }

    impl FifoBackend for MockBackend {
        fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Written)
        }
    }

    struct MockDiscoverer {
        scan_count: Arc<AtomicUsize>,
        connect_called: Arc<AtomicBool>,
        prefix: String,
        devices_to_return: Vec<(u32, Option<IpAddr>)>,
    }

    impl MockDiscoverer {
        fn new(devices: Vec<(u32, Option<IpAddr>)>) -> Self {
            Self {
                scan_count: Arc::new(AtomicUsize::new(0)),
                connect_called: Arc::new(AtomicBool::new(false)),
                prefix: "mockdac".to_string(),
                devices_to_return: devices,
            }
        }

        fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
            self.prefix = prefix.into();
            self
        }
    }

    impl Discoverer for MockDiscoverer {
        fn dac_type(&self) -> DacType {
            DacType::Custom("MockDAC".into())
        }

        fn prefix(&self) -> &str {
            &self.prefix
        }

        fn scan(&mut self) -> Vec<DiscoveredDevice> {
            self.scan_count.fetch_add(1, Ordering::SeqCst);
            self.devices_to_return
                .iter()
                .map(|(id, ip)| {
                    let hardware_name = format!("Mock Device {}", id);
                    let stable_id = match ip {
                        Some(addr) => format!("{}:{}", self.prefix, addr),
                        None => format!("{}:{}", self.prefix, id),
                    };
                    let mut info = DiscoveredDeviceInfo::new(
                        DacType::Custom("MockDAC".into()),
                        stable_id,
                        &hardware_name,
                    )
                    .with_hardware_name(hardware_name);
                    if let Some(addr) = ip {
                        info = info.with_ip(*addr);
                    }
                    DiscoveredDevice::new(info, Box::new(MockConnectionInfo { _device_id: *id }))
                })
                .collect()
        }

        fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
            self.connect_called.store(true, Ordering::SeqCst);
            let _ = downcast_connect_data::<MockConnectionInfo>(opaque, "MockDAC")?;
            Ok(BackendKind::Fifo(Box::new(MockBackend {
                connected: false,
            })))
        }
    }

    #[test]
    fn slugify_collapses_whitespace_and_punctuation() {
        assert_eq!(slugify_device_id("MOTU AVB Main"), "motu-avb-main");
        assert_eq!(slugify_device_id("  Hello, World!  "), "hello-world");
        assert_eq!(slugify_device_id("Built-in Output"), "built-in-output");
    }

    #[test]
    fn discoverer_scan_is_called() {
        let discoverer = MockDiscoverer::new(vec![(1, Some("10.0.0.1".parse().unwrap()))]);
        let scan_count = discoverer.scan_count.clone();

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer));

        assert_eq!(scan_count.load(Ordering::SeqCst), 0);
        let devices = discovery.scan();
        assert_eq!(scan_count.load(Ordering::SeqCst), 1);
        assert_eq!(devices.len(), 1);
    }

    #[test]
    fn discoverer_device_info_is_populated() {
        let discoverer = MockDiscoverer::new(vec![(42, Some("192.168.1.100".parse().unwrap()))]);

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer));

        let devices = discovery.scan();
        assert_eq!(devices.len(), 1);

        let device = &devices[0];
        assert_eq!(device.dac_type(), &DacType::Custom("MockDAC".into()));
        assert_eq!(
            device.info().ip_address,
            Some("192.168.1.100".parse().unwrap())
        );
        assert_eq!(device.info().hardware_name, Some("Mock Device 42".into()));
        assert_eq!(device.info().stable_id, "mockdac:192.168.1.100");
    }

    #[test]
    fn discoverer_connect_dispatches_to_owning_registry() {
        let discoverer = MockDiscoverer::new(vec![(99, None)]);
        let connect_called = discoverer.connect_called.clone();

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer));

        let devices = discovery.scan();
        assert_eq!(devices.len(), 1);
        assert!(!connect_called.load(Ordering::SeqCst));

        let backend = discovery.connect(devices.into_iter().next().unwrap());
        assert!(backend.is_ok());
        assert!(connect_called.load(Ordering::SeqCst));

        let backend = backend.unwrap();
        assert_eq!(backend.dac_type(), DacType::Custom("MockDAC".into()));
    }

    #[test]
    fn multiple_discoverers_with_distinct_prefixes() {
        let discoverer1 = MockDiscoverer::new(vec![(1, None)]).with_prefix("mock-a");
        let discoverer2 = MockDiscoverer::new(vec![(2, None), (3, None)]).with_prefix("mock-b");

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer1));
        discovery.register(Box::new(discoverer2));

        let devices = discovery.scan();
        assert_eq!(devices.len(), 3);
    }

    #[test]
    #[should_panic(expected = "duplicate discoverer prefix")]
    fn registering_duplicate_prefix_panics() {
        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(MockDiscoverer::new(vec![]).with_prefix("dup")));
        discovery.register(Box::new(MockDiscoverer::new(vec![]).with_prefix("dup")));
    }

    #[test]
    #[should_panic(expected = "prefix must not be empty")]
    fn registering_empty_prefix_panics() {
        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(MockDiscoverer::new(vec![]).with_prefix("")));
    }

    #[test]
    fn info_eq_and_hash_use_only_stable_id() {
        let mut a = DiscoveredDeviceInfo::new(DacType::Custom("X".into()), "x:1", "first")
            .with_ip("10.0.0.1".parse().unwrap());
        let mut b = a.clone();
        b.name = "second".into();
        b.ip_address = Some("10.0.0.2".parse().unwrap());
        assert_eq!(a, b);

        b.stable_id = "x:2".into();
        assert_ne!(a, b);

        use std::collections::hash_map::DefaultHasher;
        a.ip_address = Some("172.16.0.1".parse().unwrap());
        b.stable_id = a.stable_id.clone();
        let mut h1 = DefaultHasher::new();
        a.hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        b.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }
}
