//! Background worker for non-blocking DAC device discovery.
//!
//! The `DacDiscoveryWorker` runs discovery in a background thread and produces
//! ready-to-use `DacWorker` instances as devices are found.
//!
//! # Example
//!
//! ```ignore
//! use laser_dac::{DacDiscoveryWorker, EnabledDacTypes, LaserFrame};
//! use std::time::Duration;
//! use std::thread;
//!
//! // Create discovery worker
//! let discovery = DacDiscoveryWorker::new(EnabledDacTypes::all());
//!
//! // Collect workers as they're discovered
//! let mut workers = Vec::new();
//! for _ in 0..50 {
//!     for worker in discovery.poll_new_workers() {
//!         println!("Found: {}", worker.device_name());
//!         workers.push(worker);
//!     }
//!     thread::sleep(Duration::from_millis(100));
//! }
//! ```

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::discovery::DacDiscovery;
use crate::discovery::DiscoveredDevice;
use crate::types::EnabledDacTypes;
use crate::worker::{DacWorker, DisconnectNotifier};

type DeviceFilter = dyn Fn(&DiscoveredDevice) -> bool + Send + Sync + 'static;

/// How often to scan for DAC devices.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);

fn allow_all_devices(_: &DiscoveredDevice) -> bool {
    true
}

/// Background worker that discovers DAC devices without blocking the main thread.
///
/// USB enumeration and device opening happen in a dedicated thread. Newly discovered
/// devices are sent back as ready-to-use `DacWorker` handles.
///
/// Discovery can be configured to only scan for specific DAC types.
pub struct DacDiscoveryWorker {
    worker_rx: Receiver<DacWorker>,
    enabled_types: Arc<Mutex<EnabledDacTypes>>,
    device_filter: Arc<Mutex<Arc<DeviceFilter>>>,
    disconnect_tx: Sender<String>,
    _running: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
}

impl DacDiscoveryWorker {
    /// Creates a new discovery worker with the given enabled DAC types.
    ///
    /// This initializes USB controllers, so it should be called from the main thread.
    pub fn new(enabled_types: EnabledDacTypes) -> Self {
        Self::new_with_device_filter(enabled_types, allow_all_devices)
    }

    /// Creates a new discovery worker with a device filter predicate.
    ///
    /// Devices that do not match the predicate will be ignored.
    pub fn new_with_device_filter<F>(enabled_types: EnabledDacTypes, filter: F) -> Self
    where
        F: Fn(&DiscoveredDevice) -> bool + Send + Sync + 'static,
    {
        let (worker_tx, worker_rx) = mpsc::channel::<DacWorker>();
        let (disconnect_tx, disconnect_rx) = mpsc::channel::<String>();
        let enabled_types = Arc::new(Mutex::new(enabled_types));
        let enabled_types_clone = Arc::clone(&enabled_types);
        let device_filter = Arc::new(Mutex::new(Arc::new(filter) as Arc<DeviceFilter>));
        let device_filter_clone = Arc::clone(&device_filter);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        // Create dac discovery on main thread (USB controller init)
        let discovery =
            DacDiscovery::new(enabled_types.lock().map(|e| e.clone()).unwrap_or_default());

        let disconnect_tx_for_loop = disconnect_tx.clone();
        let handle = thread::spawn(move || {
            Self::discovery_loop(
                discovery,
                worker_tx,
                disconnect_tx_for_loop,
                disconnect_rx,
                enabled_types_clone,
                device_filter_clone,
                running_clone,
            );
        });

        Self {
            worker_rx,
            enabled_types,
            device_filter,
            disconnect_tx,
            _running: running,
            _handle: handle,
        }
    }

    /// Polls for newly discovered DAC workers.
    ///
    /// Returns an iterator of `DacWorker` handles that were discovered since the
    /// last call. These workers are already connected and ready to receive frames.
    pub fn poll_new_workers(&self) -> impl Iterator<Item = DacWorker> + '_ {
        std::iter::from_fn(|| self.worker_rx.try_recv().ok())
    }

    /// Updates the enabled DAC types.
    ///
    /// Changes take effect on the next discovery cycle.
    pub fn set_enabled_types(&self, types: EnabledDacTypes) {
        if let Ok(mut enabled) = self.enabled_types.lock() {
            *enabled = types;
        }
    }

    /// Returns the currently enabled DAC types.
    pub fn enabled_types(&self) -> EnabledDacTypes {
        self.enabled_types
            .lock()
            .map(|e| e.clone())
            .unwrap_or_default()
    }

    /// Updates the device filter predicate.
    ///
    /// Changes take effect on the next discovery cycle.
    pub fn set_device_filter<F>(&self, filter: F)
    where
        F: Fn(&DiscoveredDevice) -> bool + Send + Sync + 'static,
    {
        if let Ok(mut device_filter) = self.device_filter.lock() {
            *device_filter = Arc::new(filter);
        }
    }

    /// Clears the device filter, allowing all discovered devices.
    pub fn clear_device_filter(&self) {
        if let Ok(mut device_filter) = self.device_filter.lock() {
            *device_filter = Arc::new(allow_all_devices);
        }
    }

    /// Marks a device as disconnected, allowing it to be rediscovered.
    ///
    /// Call this when a `DacWorker` enters the `DacConnectionState::Lost` state
    /// to allow the discovery worker to reconnect to the device.
    pub fn mark_disconnected(&self, device_name: &str) {
        let _ = self.disconnect_tx.send(device_name.to_string());
    }

    /// The main discovery loop that runs in the background thread.
    fn discovery_loop(
        mut discovery: DacDiscovery,
        worker_tx: mpsc::Sender<DacWorker>,
        disconnect_tx: DisconnectNotifier,
        disconnect_rx: Receiver<String>,
        enabled_types: Arc<Mutex<EnabledDacTypes>>,
        device_filter: Arc<Mutex<Arc<DeviceFilter>>>,
        running: Arc<AtomicBool>,
    ) {
        let mut known_devices: HashSet<String> = HashSet::new();
        let mut last_discovery = Instant::now() - DISCOVERY_INTERVAL;

        while running.load(Ordering::Relaxed) {
            // Sleep until next discovery interval
            let elapsed = last_discovery.elapsed();
            if elapsed < DISCOVERY_INTERVAL {
                thread::sleep(DISCOVERY_INTERVAL - elapsed);
            }
            last_discovery = Instant::now();

            // Process disconnect notifications
            while let Ok(device_name) = disconnect_rx.try_recv() {
                known_devices.remove(&device_name);
            }

            // Update enabled types
            if let Ok(enabled) = enabled_types.lock() {
                discovery.set_enabled(enabled.clone());
            }

            // Scan for devices
            let devices = discovery.scan();
            let filter = device_filter
                .lock()
                .map(|f| Arc::clone(&*f))
                .unwrap_or_else(|_| Arc::new(allow_all_devices));
            for device in devices {
                let name = device.name().to_string();
                let dac_type = device.dac_type();

                // Skip already-known devices
                if known_devices.contains(&name) {
                    continue;
                }

                // Skip filtered devices
                if !filter(&device) {
                    continue;
                }

                // Try to connect
                match discovery.connect(device) {
                    Ok(backend) => {
                        let worker = DacWorker::new_with_disconnect_notifier(
                            name.clone(),
                            dac_type,
                            backend,
                            disconnect_tx.clone(),
                        );
                        if worker_tx.send(worker).is_err() {
                            // Receiver dropped, exit
                            return;
                        }
                        known_devices.insert(name);
                    }
                    Err(_) => {
                        // Connection failed, will retry on next scan
                    }
                }
            }
        }
    }
}

impl Default for DacDiscoveryWorker {
    fn default() -> Self {
        Self::new(EnabledDacTypes::default())
    }
}
