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
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::discovery::UnifiedDiscovery;
use crate::types::EnabledDacTypes;
use crate::worker::DacWorker;

/// How often to scan for DAC devices.
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(2);

/// Background worker that discovers DAC devices without blocking the main thread.
///
/// USB enumeration and device opening happen in a dedicated thread. Newly discovered
/// devices are sent back as ready-to-use `DacWorker` handles.
///
/// Discovery can be configured to only scan for specific DAC types.
pub struct DacDiscoveryWorker {
    worker_rx: Receiver<DacWorker>,
    enabled_types: Arc<Mutex<EnabledDacTypes>>,
    _running: Arc<AtomicBool>,
    _handle: JoinHandle<()>,
}

impl DacDiscoveryWorker {
    /// Creates a new discovery worker with the given enabled DAC types.
    ///
    /// This initializes USB controllers, so it should be called from the main thread.
    pub fn new(enabled_types: EnabledDacTypes) -> Self {
        let (worker_tx, worker_rx) = mpsc::channel::<DacWorker>();
        let enabled_types = Arc::new(Mutex::new(enabled_types));
        let enabled_types_clone = Arc::clone(&enabled_types);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = Arc::clone(&running);

        // Create unified discovery on main thread (USB controller init)
        let discovery =
            UnifiedDiscovery::new(enabled_types.lock().map(|e| e.clone()).unwrap_or_default());

        let handle = thread::spawn(move || {
            Self::discovery_loop(discovery, worker_tx, enabled_types_clone, running_clone);
        });

        Self {
            worker_rx,
            enabled_types,
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

    /// The main discovery loop that runs in the background thread.
    fn discovery_loop(
        mut discovery: UnifiedDiscovery,
        worker_tx: mpsc::Sender<DacWorker>,
        enabled_types: Arc<Mutex<EnabledDacTypes>>,
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

            // Update enabled types
            if let Ok(enabled) = enabled_types.lock() {
                discovery.set_enabled(enabled.clone());
            }

            // Scan for devices
            let devices = discovery.scan();

            for device in devices {
                let name = device.name().to_string();
                let dac_type = device.dac_type();

                // Skip already-known devices
                if known_devices.contains(&name) {
                    continue;
                }

                // Try to connect
                match discovery.connect(device) {
                    Ok(backend) => {
                        let worker = DacWorker::new(name.clone(), dac_type, backend);
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
