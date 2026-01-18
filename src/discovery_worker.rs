//! Background worker for non-blocking DAC discovery.
//!
//! The `DacDiscoveryWorker` runs discovery in a background thread and produces
//! ready-to-use [`Dac`] instances as DACs are found.

use std::collections::HashMap;
#[cfg(all(feature = "idn", feature = "testutils"))]
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::discovery::{CustomDiscoverySource, DacDiscovery, DiscoveredDeviceInfo};
use crate::stream::Dac;
use crate::types::{DacInfo, EnabledDacTypes};

type DeviceFilter = dyn Fn(&DiscoveredDeviceInfo) -> bool + Send + Sync + 'static;

/// How often to scan for DAC devices.
const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);

/// How long before a device that hasn't been seen is considered gone and can be rediscovered.
/// This should be longer than the discovery interval to avoid spurious rediscoveries.
const DEVICE_TTL: Duration = Duration::from_secs(10);

fn allow_all_devices(_: &DiscoveredDeviceInfo) -> bool {
    true
}

/// Builder for `DacDiscoveryWorker`.
///
/// Use `DacDiscoveryWorker::builder()` to create a new builder.
pub struct DacDiscoveryWorkerBuilder {
    enabled_types: EnabledDacTypes,
    device_filter: Option<Arc<DeviceFilter>>,
    discovery_interval: Duration,
    custom_sources: Vec<Box<dyn CustomDiscoverySource>>,
    #[cfg(all(feature = "idn", feature = "testutils"))]
    idn_scan_addresses: Vec<SocketAddr>,
}

impl DacDiscoveryWorkerBuilder {
    /// Creates a new builder with default settings.
    pub fn new() -> Self {
        Self {
            enabled_types: EnabledDacTypes::default(),
            device_filter: None,
            discovery_interval: DEFAULT_DISCOVERY_INTERVAL,
            custom_sources: Vec::new(),
            #[cfg(all(feature = "idn", feature = "testutils"))]
            idn_scan_addresses: Vec::new(),
        }
    }

    /// Sets which DAC types to scan for.
    pub fn enabled_types(mut self, types: EnabledDacTypes) -> Self {
        self.enabled_types = types;
        self
    }

    /// Sets a filter predicate for auto-connecting to devices.
    ///
    /// Devices that return `false` from the filter will still be reported via
    /// `poll_discovered_devices()`, but will not be automatically connected
    /// (and thus won't appear in `poll_new_devices()`).
    pub fn device_filter<F>(mut self, filter: F) -> Self
    where
        F: Fn(&DiscoveredDeviceInfo) -> bool + Send + Sync + 'static,
    {
        self.device_filter = Some(Arc::new(filter));
        self
    }

    /// Sets the discovery scan interval.
    ///
    /// Defaults to 2 seconds.
    pub fn discovery_interval(mut self, interval: Duration) -> Self {
        self.discovery_interval = interval;
        self
    }

    /// Adds a custom discovery source.
    ///
    /// Custom sources are polled alongside built-in backends at the configured
    /// discovery interval. You can add multiple custom sources by calling this
    /// method multiple times.
    pub fn add_custom_source<S: CustomDiscoverySource>(mut self, source: S) -> Self {
        self.custom_sources.push(Box::new(source));
        self
    }

    /// Sets specific addresses to scan for IDN servers.
    ///
    /// When set, the scanner will scan these specific addresses instead of
    /// using broadcast discovery. This is useful for testing with mock servers
    /// on localhost.
    ///
    /// This method is only available with the `testutils` feature.
    #[cfg(all(feature = "idn", feature = "testutils"))]
    pub fn idn_scan_addresses(mut self, addresses: Vec<SocketAddr>) -> Self {
        self.idn_scan_addresses = addresses;
        self
    }

    /// Builds the `DacDiscoveryWorker`.
    ///
    /// This initializes USB controllers, so it should be called from the main thread.
    pub fn build(self) -> DacDiscoveryWorker {
        let (device_out_tx, device_out_rx) = mpsc::channel::<Dac>();
        let (device_info_tx, device_info_rx) = mpsc::channel::<DiscoveredDeviceInfo>();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        let device_filter = self
            .device_filter
            .unwrap_or_else(|| Arc::new(allow_all_devices));

        // Create dac discovery on main thread (USB controller init)
        #[allow(unused_mut)] // mut only needed with testutils feature
        let mut discovery = DacDiscovery::new(self.enabled_types.clone());

        // Set IDN scan addresses if configured
        #[cfg(all(feature = "idn", feature = "testutils"))]
        if !self.idn_scan_addresses.is_empty() {
            discovery.set_idn_scan_addresses(self.idn_scan_addresses);
        }

        let handle = thread::spawn(move || {
            discovery_loop(
                discovery,
                device_out_tx,
                device_info_tx,
                self.enabled_types,
                device_filter,
                self.discovery_interval,
                running_clone,
                self.custom_sources,
            );
        });

        DacDiscoveryWorker {
            device_rx: device_out_rx,
            device_info_rx,
            running,
            handle: Some(handle),
        }
    }
}

impl Default for DacDiscoveryWorkerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Background worker that discovers DAC devices without blocking the main thread.
///
/// USB enumeration and device opening happen in a dedicated thread. All discovered
/// devices are reported via `poll_discovered_devices()`, while only devices that
/// pass the filter are automatically connected and available via `poll_new_devices()`.
///
/// Use `DacDiscoveryWorker::builder()` to create and configure a new worker.
///
/// The background thread is automatically stopped when the worker is dropped.
pub struct DacDiscoveryWorker {
    device_rx: Receiver<Dac>,
    device_info_rx: Receiver<DiscoveredDeviceInfo>,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DacDiscoveryWorker {
    /// Creates a new builder for configuring the discovery worker.
    pub fn builder() -> DacDiscoveryWorkerBuilder {
        DacDiscoveryWorkerBuilder::new()
    }

    /// Polls for newly discovered devices.
    ///
    /// Returns an iterator of `DiscoveredDeviceInfo` for all devices discovered
    /// since the last call, regardless of whether they pass the device filter.
    pub fn poll_discovered_devices(&self) -> impl Iterator<Item = DiscoveredDeviceInfo> + '_ {
        std::iter::from_fn(|| self.device_info_rx.try_recv().ok())
    }

    /// Polls for newly connected DACs.
    ///
    /// Returns an iterator of [`Dac`] handles for DACs that were discovered
    /// and passed the filter since the last call. These DACs are already
    /// connected and ready to start streaming.
    pub fn poll_new_devices(&self) -> impl Iterator<Item = Dac> + '_ {
        std::iter::from_fn(|| self.device_rx.try_recv().ok())
    }
}

impl Default for DacDiscoveryWorker {
    fn default() -> Self {
        DacDiscoveryWorkerBuilder::new().build()
    }
}

impl Drop for DacDiscoveryWorker {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The main discovery loop that runs in the background thread.
#[allow(clippy::too_many_arguments)]
fn discovery_loop(
    mut discovery: DacDiscovery,
    device_tx: Sender<Dac>,
    device_info_tx: Sender<DiscoveredDeviceInfo>,
    enabled_types: EnabledDacTypes,
    device_filter: Arc<DeviceFilter>,
    discovery_interval: Duration,
    running: Arc<AtomicBool>,
    mut custom_sources: Vec<Box<dyn CustomDiscoverySource>>,
) {
    // Track devices by their stable ID and when they were last seen.
    // Devices that haven't been seen for DEVICE_TTL are removed, allowing rediscovery.
    let mut reported_devices: HashMap<String, Instant> = HashMap::new();
    let mut last_discovery = Instant::now() - discovery_interval;

    // Set initial enabled types
    discovery.set_enabled(enabled_types);

    while running.load(Ordering::Relaxed) {
        // Sleep until next discovery interval
        let elapsed = last_discovery.elapsed();
        if elapsed < discovery_interval {
            thread::sleep(discovery_interval - elapsed);
        }
        let now = Instant::now();
        last_discovery = now;

        // Prune devices that haven't been seen for DEVICE_TTL
        reported_devices.retain(|_, last_seen| now.duration_since(*last_seen) < DEVICE_TTL);

        // Scan for built-in devices
        let devices = discovery.scan();
        for device in devices {
            let info = device.info();
            let stable_id = info.stable_id();

            // Update last-seen time for known devices, skip if already reported
            if let Some(last_seen) = reported_devices.get_mut(&stable_id) {
                *last_seen = now;
                continue;
            }

            if device_info_tx.send(info.clone()).is_err() {
                return;
            }
            reported_devices.insert(stable_id.clone(), now);

            // Only connect if filter passes
            if !device_filter(&info) {
                continue;
            }

            let backend = match discovery.connect(device) {
                Ok(b) => b,
                Err(_) => {
                    reported_devices.remove(&stable_id);
                    continue;
                }
            };

            let device_info = DacInfo {
                id: stable_id.clone(),
                name: info.name(),
                kind: info.dac_type,
                caps: backend.caps().clone(),
            };
            let device = Dac::new(device_info, backend);
            if device_tx.send(device).is_err() {
                return;
            }
        }

        // Poll custom discovery sources
        for source in &mut custom_sources {
            let custom_devs = source.poll_devices();
            for dac in custom_devs {
                let stable_id = dac.id.clone();

                // Update last-seen time for known devices, skip if already reported
                if let Some(last_seen) = reported_devices.get_mut(&stable_id) {
                    *last_seen = now;
                    continue;
                }

                let info: DiscoveredDeviceInfo = (&dac).into();

                if device_info_tx.send(info.clone()).is_err() {
                    return;
                }
                reported_devices.insert(stable_id.clone(), now);

                // Only connect if filter passes
                if !device_filter(&info) {
                    continue;
                }

                let Some(backend) = source.create_backend(&dac) else {
                    reported_devices.remove(&stable_id);
                    continue;
                };

                let device_info = DacInfo {
                    id: stable_id.clone(),
                    name: dac.name.clone(),
                    kind: info.dac_type.clone(),
                    caps: backend.caps().clone(),
                };
                let device = Dac::new(device_info, backend);
                if device_tx.send(device).is_err() {
                    return;
                }
            }
        }
    }
}
