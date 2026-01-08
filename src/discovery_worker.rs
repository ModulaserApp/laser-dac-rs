//! Background worker for non-blocking DAC device discovery.
//!
//! The `DacDiscoveryWorker` runs discovery in a background thread and produces
//! ready-to-use worker instances as devices are found.
//!
//! By default, the discovery worker produces [`DacWorker`] instances. Use
//! [`use_callback_workers()`](DacDiscoveryWorkerBuilder::use_callback_workers) to get
//! [`DacCallbackWorker`] instances instead, where the DAC drives timing via callbacks.

use std::collections::HashSet;
#[cfg(all(feature = "idn", feature = "testutils"))]
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::discovery::CustomDiscoverySource;
use crate::discovery::DacDiscovery;
use crate::discovery::DiscoveredDeviceInfo;
use crate::types::{DiscoveredDac, EnabledDacTypes};
use crate::worker::{DacCallbackWorker, DacWorker, DisconnectNotifier};

type DeviceFilter = dyn Fn(&DiscoveredDeviceInfo) -> bool + Send + Sync + 'static;

/// How often to scan for DAC devices.
const DEFAULT_DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);

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
    use_callback_workers: bool,
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
            use_callback_workers: false,
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
    /// (and thus won't appear in `poll_new_workers()`).
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

    /// Produce [`DacCallbackWorker`] instances instead of [`DacWorker`].
    ///
    /// When enabled, use [`poll_new_callback_workers()`](DacDiscoveryWorker::poll_new_callback_workers)
    /// instead of [`poll_new_workers()`](DacDiscoveryWorker::poll_new_workers).
    ///
    /// The callback workers are created in an idle state - you must call
    /// [`DacCallbackWorker::start()`] on each one to begin the callback loop.
    pub fn use_callback_workers(mut self) -> Self {
        self.use_callback_workers = true;
        self
    }

    /// Adds a custom discovery source.
    ///
    /// Custom sources are polled alongside built-in backends at the configured
    /// discovery interval. You can add multiple custom sources by calling this
    /// method multiple times.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use laser_dac::{DacDiscoveryWorker, CustomDiscoverySource};
    ///
    /// let discovery = DacDiscoveryWorker::builder()
    ///     .add_custom_source(MyCustomSource::new())
    ///     .build();
    /// ```
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
        let (worker_tx, worker_rx) = mpsc::channel::<DacWorker>();
        let (device_tx, device_rx) = mpsc::channel::<DiscoveredDeviceInfo>();
        let (disconnect_tx, disconnect_rx) = mpsc::channel::<String>();
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        // Create callback worker channel if requested
        let (callback_worker_tx, callback_worker_rx) = if self.use_callback_workers {
            let (tx, rx) = mpsc::channel::<DacCallbackWorker>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

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

        let disconnect_tx_for_loop = disconnect_tx.clone();
        let enabled_types = self.enabled_types;
        let discovery_interval = self.discovery_interval;
        let custom_sources = self.custom_sources;

        let handle = thread::spawn(move || {
            discovery_loop(
                discovery,
                worker_tx,
                callback_worker_tx,
                device_tx,
                disconnect_tx_for_loop,
                disconnect_rx,
                enabled_types,
                device_filter,
                discovery_interval,
                running_clone,
                custom_sources,
            );
        });

        DacDiscoveryWorker {
            worker_rx,
            callback_worker_rx,
            device_rx,
            running,
            handle: Some(handle),
            callback_mode: self.use_callback_workers,
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
/// pass the filter are automatically connected and available via `poll_new_workers()`
/// or `poll_new_callback_workers()` (depending on builder configuration).
///
/// Use `DacDiscoveryWorker::builder()` to create and configure a new worker.
///
/// The background thread is automatically stopped when the worker is dropped.
pub struct DacDiscoveryWorker {
    worker_rx: Receiver<DacWorker>,
    callback_worker_rx: Option<Receiver<DacCallbackWorker>>,
    device_rx: Receiver<DiscoveredDeviceInfo>,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Whether callback mode is enabled (for runtime validation).
    callback_mode: bool,
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
        std::iter::from_fn(|| self.device_rx.try_recv().ok())
    }

    /// Polls for newly connected DAC workers.
    ///
    /// Returns an iterator of `DacWorker` handles for devices that were discovered
    /// and passed the device filter since the last call. These workers are already
    /// connected and ready to receive frames.
    ///
    /// # Panics
    ///
    /// Panics if called when [`use_callback_workers()`](DacDiscoveryWorkerBuilder::use_callback_workers)
    /// was enabled. Use [`poll_new_callback_workers()`](Self::poll_new_callback_workers) instead.
    pub fn poll_new_workers(&self) -> impl Iterator<Item = DacWorker> + '_ {
        assert!(
            !self.callback_mode,
            "poll_new_workers() called but use_callback_workers() was enabled; \
             use poll_new_callback_workers() instead"
        );
        std::iter::from_fn(|| self.worker_rx.try_recv().ok())
    }

    /// Polls for newly connected callback workers.
    ///
    /// Returns an iterator of `DacCallbackWorker` handles for devices that were
    /// discovered and passed the device filter since the last call.
    ///
    /// These workers are connected but **not yet started** - you must call
    /// [`DacCallbackWorker::start()`] on each one to begin the callback loop.
    ///
    /// # Panics
    ///
    /// Panics if called when [`use_callback_workers()`](DacDiscoveryWorkerBuilder::use_callback_workers)
    /// was **not** enabled. Use [`poll_new_workers()`](Self::poll_new_workers) instead.
    pub fn poll_new_callback_workers(&self) -> impl Iterator<Item = DacCallbackWorker> + '_ {
        assert!(
            self.callback_mode,
            "poll_new_callback_workers() called but use_callback_workers() was not enabled; \
             use poll_new_workers() instead"
        );
        std::iter::from_fn(|| {
            self.callback_worker_rx
                .as_ref()
                .and_then(|rx| rx.try_recv().ok())
        })
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
    worker_tx: Sender<DacWorker>,
    callback_worker_tx: Option<Sender<DacCallbackWorker>>,
    device_tx: Sender<DiscoveredDeviceInfo>,
    disconnect_tx: DisconnectNotifier,
    disconnect_rx: Receiver<String>,
    enabled_types: EnabledDacTypes,
    device_filter: Arc<DeviceFilter>,
    discovery_interval: Duration,
    running: Arc<AtomicBool>,
    mut custom_sources: Vec<Box<dyn CustomDiscoverySource>>,
) {
    let mut reported_devices: HashSet<String> = HashSet::new();
    let mut last_discovery = Instant::now() - discovery_interval;

    // Store custom devices for connection attempts (id -> DiscoveredDac)
    let mut custom_devices: std::collections::HashMap<String, DiscoveredDac> =
        std::collections::HashMap::new();

    // Set initial enabled types
    discovery.set_enabled(enabled_types);

    while running.load(Ordering::Relaxed) {
        // Sleep until next discovery interval
        let elapsed = last_discovery.elapsed();
        if elapsed < discovery_interval {
            thread::sleep(discovery_interval - elapsed);
        }
        last_discovery = Instant::now();

        // Process disconnect notifications
        while let Ok(device_name) = disconnect_rx.try_recv() {
            reported_devices.remove(&device_name);
        }

        // Scan for built-in devices
        let devices = discovery.scan();
        for device in devices {
            let info = device.info();

            let name = info.name();

            // Skip already-reported devices
            if reported_devices.contains(&name) {
                continue;
            }

            // Always report discovered device (once)
            if device_tx.send(info.clone()).is_err() {
                // Receiver dropped, exit
                return;
            }
            reported_devices.insert(name.clone());

            // Only connect if filter passes
            if !device_filter(&info) {
                continue;
            }

            // Try to connect
            match discovery.connect(device) {
                Ok(backend) => {
                    // Create callback worker or regular worker depending on mode
                    if let Some(ref tx) = callback_worker_tx {
                        let worker = DacCallbackWorker::new_with_disconnect_notifier(
                            name.clone(),
                            info.dac_type,
                            backend,
                            disconnect_tx.clone(),
                        );
                        if tx.send(worker).is_err() {
                            // Receiver dropped, exit
                            return;
                        }
                    } else {
                        let worker = DacWorker::new_with_disconnect_notifier(
                            name.clone(),
                            info.dac_type,
                            backend,
                            disconnect_tx.clone(),
                        );
                        if worker_tx.send(worker).is_err() {
                            // Receiver dropped, exit
                            return;
                        }
                    }
                }
                Err(_) => {
                    // Connection failed, remove from reported so we can retry next cycle
                    reported_devices.remove(&name);
                }
            }
        }

        // Poll custom discovery sources
        for source in &mut custom_sources {
            let custom_devs = source.poll_devices();
            for dac in custom_devs {
                let id = dac.id.clone();

                // Skip already-reported devices
                if reported_devices.contains(&id) {
                    continue;
                }

                // Convert to DiscoveredDeviceInfo for reporting and filtering
                let info: DiscoveredDeviceInfo = (&dac).into();

                // Always report discovered device (once)
                if device_tx.send(info.clone()).is_err() {
                    // Receiver dropped, exit
                    return;
                }
                reported_devices.insert(id.clone());

                // Store the device for connection
                custom_devices.insert(id.clone(), dac);

                // Only connect if filter passes
                if !device_filter(&info) {
                    continue;
                }

                // Try to connect via the custom source
                if let Some(dac) = custom_devices.get(&id) {
                    if let Some(backend) = source.create_backend(dac) {
                        // Create callback worker or regular worker depending on mode
                        if let Some(ref tx) = callback_worker_tx {
                            let worker = DacCallbackWorker::new_with_disconnect_notifier(
                                id.clone(),
                                info.dac_type.clone(),
                                backend,
                                disconnect_tx.clone(),
                            );
                            if tx.send(worker).is_err() {
                                // Receiver dropped, exit
                                return;
                            }
                        } else {
                            let worker = DacWorker::new_with_disconnect_notifier(
                                id.clone(),
                                info.dac_type.clone(),
                                backend,
                                disconnect_tx.clone(),
                            );
                            if worker_tx.send(worker).is_err() {
                                // Receiver dropped, exit
                                return;
                            }
                        }
                    } else {
                        // Connection failed, remove from reported so we can retry next cycle
                        reported_devices.remove(&id);
                    }
                }
            }
        }
    }
}
