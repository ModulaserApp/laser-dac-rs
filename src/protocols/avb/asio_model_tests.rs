//! AVB end-to-end over a model of the Windows ASIO stack.
//!
//! The other AVB tests replace the whole audio engine, so they never exercise
//! enumeration, device selection, or the worker-thread stream open. That gap
//! hid a Windows bug where an RME MADIface USB was found during connect on the
//! calling thread and then "not found among candidates" on the audio worker
//! thread, every time.
//!
//! These tests run the production engine ([`HostAudioEngine`]) through the
//! public `DacDiscovery` → `BackendKind` flow, threaded the way Modulaser
//! threads it (scan on a discovery thread, connect on a pipeline thread, stream
//! on the AVB worker). Only the OS audio host is replaced, by [`ModelAsio`],
//! which enforces what the real ASIO SDK and asio-sys impose:
//!
//! - Loading a driver needs a COM apartment on the calling thread. The one
//!   exception is the process's first load: the SDK then calls `CoInitialize`
//!   itself, on that thread, and never undoes it. A failed load silently drops
//!   the device, as cpal does.
//! - One host instance tracks one loaded driver: loading a different one
//!   while it is alive fails (asio-sys `DriverAlreadyExists`), again dropping
//!   the device silently.
//! - The SDK has a single global driver slot. A load that reaches it while
//!   another driver is live unloads that driver.
//! - SDK calls must not overlap across threads.
//! - A driver must be released before its creating thread leaves the
//!   apartment it was created in.
//! - A driver runs one stream. Opening a second one through the same loaded
//!   driver (which asio-sys hands out for a matching name) disposes and
//!   replaces the buffers the first stream's callback is still using.
//!
//! Breaking the last four is recorded as a violation instead of panicking
//! (it can happen on driver or worker threads), and every test asserts none
//! occurred.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::apartment::model as com;
use super::backend::{
    fill_output_buffer, AudioEngine, AvbSelector, HostAudioEngine, OutputConfigRange, RuntimeState,
    SelectedStreamConfig,
};
use super::discovery::AvbDiscoverer;
use super::host::{AudioHost, HostAccess, OutputDevice};
use crate::backend::BackendKind;
use crate::device::EnabledDacTypes;
use crate::discovery::{DacDiscovery, DiscoveredDevice};
use crate::error::{Error, Result};
use crate::point::LaserPoint;
use crate::protocols::audio_sink::RunningAudioStream;
use cpal::SampleFormat;

const MADIFACE: DriverSpec = DriverSpec {
    name: "ASIO MADIface USB",
    channels: 5,
    sample_rate: 48_000,
};
const ASIO4ALL: DriverSpec = DriverSpec {
    name: "ASIO4ALL v2",
    channels: 8,
    sample_rate: 44_100,
};
const REALTEK: DriverSpec = DriverSpec {
    name: "Realtek ASIO",
    channels: 2,
    sample_rate: 48_000,
};

const SCENARIO_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Clone, Copy)]
struct DriverSpec {
    name: &'static str,
    channels: u16,
    sample_rate: u32,
}

/// Process-wide ASIO SDK state (one per test, standing in for one process).
struct Sdk {
    drivers: Vec<DriverSpec>,
    first_load_done: AtomicBool,
    /// Generation of the driver in the SDK's global slot.
    current: Mutex<Option<u64>>,
    next_generation: AtomicU64,
    in_call: AtomicBool,
    violations: Mutex<Vec<String>>,
    /// Frames written by the driver callback, most recent last.
    frames: Mutex<Vec<Vec<f32>>>,
    /// When set, the next stream start blocks inside the driver until the
    /// paired sender is dropped (a hung `ASIOCreateBuffers`).
    hang_next_start: Mutex<Option<mpsc::Receiver<()>>>,
}

impl Sdk {
    fn new(drivers: Vec<DriverSpec>) -> Arc<Self> {
        Arc::new(Self {
            drivers,
            first_load_done: AtomicBool::new(false),
            current: Mutex::new(None),
            next_generation: AtomicU64::new(1),
            in_call: AtomicBool::new(false),
            violations: Mutex::new(Vec::new()),
            frames: Mutex::new(Vec::new()),
            hang_next_start: Mutex::new(None),
        })
    }

    fn call<R>(&self, what: &str, f: impl FnOnce() -> R) -> R {
        if self.in_call.swap(true, Ordering::SeqCst) {
            self.violation(format!("overlapping ASIO SDK calls ({what})"));
        }
        // Widen the window so unserialized callers actually collide.
        thread::sleep(Duration::from_micros(300));
        let result = f();
        self.in_call.store(false, Ordering::SeqCst);
        result
    }

    fn violation(&self, message: String) {
        lock(&self.violations).push(message);
    }

    fn violations(&self) -> Vec<String> {
        lock(&self.violations).clone()
    }

    fn current_generation(&self) -> Option<u64> {
        *lock(&self.current)
    }

    /// Makes the next stream start hang until the returned sender is dropped.
    fn hang_next_start(&self) -> mpsc::Sender<()> {
        let (release, hang) = mpsc::channel();
        *lock(&self.hang_next_start) = Some(hang);
        release
    }

    fn clear_frames(&self) {
        lock(&self.frames).clear();
    }

    /// Whether the driver emitted a lit frame at the expected (galvo-flipped)
    /// position since the last `clear_frames`.
    fn saw_lit_frame_at(&self, x: f32, y: f32) -> bool {
        lock(&self.frames)
            .iter()
            .any(|f| f[0] == -x && f[1] == -y && f[2] > 0.0)
    }
}

/// One asio-sys `Asio` instance (what one cpal ASIO host wraps).
struct ModelAsio {
    sdk: Arc<Sdk>,
    loaded: Mutex<Weak<LoadedDriver>>,
}

struct LoadedDriver {
    sdk: Arc<Sdk>,
    spec: DriverSpec,
    generation: u64,
    apartment: u64,
    streams: AtomicUsize,
}

impl ModelAsio {
    fn load_driver(&self, spec: DriverSpec) -> Option<Arc<LoadedDriver>> {
        if let Some(driver) = lock(&self.loaded).upgrade() {
            return (driver.spec.name == spec.name).then_some(driver);
        }
        let sdk = &self.sdk;
        let driver = sdk.call("load driver", || {
            if !sdk.first_load_done.swap(true, Ordering::SeqCst) {
                // The SDK's global driver list calls CoInitialize here, once,
                // and never balances it.
                com::enter();
            }
            // CoCreateInstance fails on a thread outside any apartment.
            let apartment = com::current()?;
            let generation = sdk.next_generation.fetch_add(1, Ordering::SeqCst);
            if let Some(live) = lock(&sdk.current).replace(generation) {
                sdk.violation(format!(
                    "loading {:?} unloaded live driver generation {live}",
                    spec.name
                ));
            }
            Some(Arc::new(LoadedDriver {
                sdk: Arc::clone(sdk),
                spec,
                generation,
                apartment,
                streams: AtomicUsize::new(0),
            }))
        })?;
        *lock(&self.loaded) = Arc::downgrade(&driver);
        Some(driver)
    }
}

impl Drop for LoadedDriver {
    fn drop(&mut self) {
        self.sdk.call("unload driver", || {
            if !com::is_alive(self.apartment) {
                self.sdk.violation(format!(
                    "{:?} released after its creating thread left the COM apartment",
                    self.spec.name
                ));
            }
            let mut current = lock(&self.sdk.current);
            if *current == Some(self.generation) {
                *current = None;
            }
        });
    }
}

impl AudioHost for ModelAsio {
    type Device = ModelDevice;

    fn output_devices(&self) -> Result<Box<dyn Iterator<Item = ModelDevice> + '_>> {
        Ok(Box::new(self.sdk.drivers.iter().filter_map(|spec| {
            self.load_driver(*spec).map(|driver| ModelDevice { driver })
        })))
    }

    fn single_stream(&self) -> bool {
        true
    }
}

struct ModelDevice {
    driver: Arc<LoadedDriver>,
}

impl OutputDevice for ModelDevice {
    fn name(&self) -> Option<String> {
        Some(self.driver.spec.name.to_string())
    }

    fn output_config_ranges(&self) -> Vec<OutputConfigRange> {
        let spec = self.driver.spec;
        self.driver.sdk.call("get channels", || {
            vec![OutputConfigRange {
                channels: spec.channels,
                min_sample_rate: spec.sample_rate,
                max_sample_rate: spec.sample_rate,
                sample_format: SampleFormat::I32,
            }]
        })
    }

    fn default_output(&self) -> Option<(u16, u32)> {
        let spec = self.driver.spec;
        self.driver.sdk.call("get sample rate", || {
            Some((spec.channels, spec.sample_rate))
        })
    }

    fn start_output(
        &self,
        config: SelectedStreamConfig,
        runtime: &Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>> {
        let driver = Arc::clone(&self.driver);
        let sdk = Arc::clone(&driver.sdk);
        let started = sdk.call("create buffers + start", || {
            if let Some(hang) = lock(&sdk.hang_next_start).take() {
                let _ = hang.recv();
            }
            if driver.streams.fetch_add(1, Ordering::SeqCst) > 0 {
                sdk.violation(format!(
                    "second stream on {:?} disposed the live stream's buffers",
                    driver.spec.name
                ));
            }
            sdk.current_generation() == Some(driver.generation)
        });
        if !started {
            driver.streams.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::backend(super::error::Error::StreamStartFailed));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let callback = {
            let (driver, stop, runtime) =
                (Arc::clone(&driver), Arc::clone(&stop), Arc::clone(runtime));
            let channels = config.channels as usize;
            thread::spawn(move || run_driver_callbacks(&driver, channels, &runtime, &stop))
        };
        Ok(Box::new(ModelStream {
            stop,
            callback: Some(callback),
            driver,
        }))
    }
}

/// The driver's own realtime thread: pulls buffers until stopped, and fails
/// the stream if its driver is unloaded or loses its apartment underneath it.
fn run_driver_callbacks(
    driver: &LoadedDriver,
    channels: usize,
    runtime: &RuntimeState,
    stop: &AtomicBool,
) {
    while !stop.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(1));
        let alive = driver.sdk.current_generation() == Some(driver.generation)
            && com::is_alive(driver.apartment);
        if !alive {
            driver.sdk.violation(format!(
                "{:?} was torn down while its stream was running",
                driver.spec.name
            ));
            runtime.mark_stream_failed();
            return;
        }
        let mut buffer = vec![0.0; channels * 48];
        fill_output_buffer(&mut buffer, channels, runtime);
        let mut frames = lock(&driver.sdk.frames);
        frames.extend(buffer.chunks(channels).map(<[f32]>::to_vec));
        let excess = frames.len().saturating_sub(4096);
        frames.drain(..excess);
    }
}

struct ModelStream {
    stop: Arc<AtomicBool>,
    callback: Option<JoinHandle<()>>,
    driver: Arc<LoadedDriver>,
}

impl RunningAudioStream for ModelStream {}

impl Drop for ModelStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(callback) = self.callback.take() {
            let _ = callback.join();
        }
        self.driver.sdk.call("stop + dispose buffers", || {
            self.driver.streams.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// =============================================================================
// Scenario helpers
// =============================================================================

/// One simulated process: an ASIO SDK, one shared host, and the AVB
/// discoverer wired to it the way `AvbDiscoverer::new` wires the real one.
struct Process {
    sdk: Arc<Sdk>,
    engine: Arc<dyn AudioEngine>,
}

impl Process {
    fn with_drivers(drivers: &[DriverSpec]) -> Self {
        let sdk = Sdk::new(drivers.to_vec());
        let host = ModelAsio {
            sdk: Arc::clone(&sdk),
            loaded: Mutex::new(Weak::new()),
        };
        let access = HostAccess::with_session_timeout(host, SESSION_TIMEOUT);
        let engine = Arc::new(HostAudioEngine::new(Arc::new(access)));
        Self { sdk, engine }
    }

    fn discovery(&self) -> DacDiscovery {
        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(AvbDiscoverer::with_engine(Arc::clone(
            &self.engine,
        ))));
        discovery
    }

    fn assert_no_violations(&self) {
        let violations = self.sdk.violations();
        assert!(violations.is_empty(), "ASIO rules broken: {violations:#?}");
    }
}

/// How the thread that calls `connect` found COM when it started.
#[derive(Clone, Copy)]
enum CallerApartment {
    /// Another library on the thread already entered an apartment (in
    /// Modulaser's pipeline thread, cpal's WASAPI audio-input host does).
    AlreadyInitialized,
    None,
}

fn run_on_thread<T: Send + 'static>(
    name: &str,
    apartment: CallerApartment,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            if let CallerApartment::AlreadyInitialized = apartment {
                com::enter();
            }
            f()
        })
        .unwrap()
        .join()
        .unwrap()
}

fn scan_on_discovery_thread(discovery: DacDiscovery) -> (DacDiscovery, Vec<DiscoveredDevice>) {
    run_on_thread("dac-discovery", CallerApartment::None, move || {
        let mut discovery = discovery;
        let devices = discovery.scan();
        (discovery, devices)
    })
}

fn device_names(devices: &[DiscoveredDevice]) -> Vec<String> {
    devices
        .iter()
        .map(|d| d.info().name().to_string())
        .collect()
}

fn take_device(devices: Vec<DiscoveredDevice>, name: &str) -> DiscoveredDevice {
    devices
        .into_iter()
        .find(|d| d.info().name() == name)
        .unwrap_or_else(|| panic!("{name:?} was not discovered"))
}

/// Writes points until the driver emits them, proving the whole path from
/// `try_write` to the device callback is live.
fn assert_streams(backend: &mut BackendKind, sdk: &Sdk, x: f32, y: f32) {
    sdk.clear_frames();
    backend.set_shutter(true).unwrap();
    let points = vec![LaserPoint::new(x, y, 65535, 0, 0, 65535); 256];
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    while !sdk.saw_lit_frame_at(x, y) {
        assert!(
            Instant::now() < deadline,
            "points written at ({x}, {y}) never reached the driver"
        );
        backend.try_write(48_000, &points).unwrap();
        thread::sleep(Duration::from_millis(2));
    }
}

fn connect(discovery: &mut DacDiscovery, device: DiscoveredDevice) -> Result<BackendKind> {
    let mut backend = discovery.connect(device)?;
    backend.connect()?;
    Ok(backend)
}

// =============================================================================
// Scenarios
// =============================================================================

/// The reported failure: discovery sees the MADIface, connect resolves it on
/// the pipeline thread, and the AVB worker thread then can't load it.
fn connect_across_threads(caller: CallerApartment) {
    let process = Process::with_drivers(&[MADIFACE]);
    let (discovery, devices) = scan_on_discovery_thread(process.discovery());
    let device = take_device(devices, MADIFACE.name);

    let sdk = Arc::clone(&process.sdk);
    run_on_thread("pipeline", caller, move || {
        let mut discovery = discovery;
        let mut backend = connect(&mut discovery, device).expect("connect");
        assert_streams(&mut backend, &sdk, 0.5, -0.25);

        // Reconnect: the released driver must load again on a fresh worker.
        backend.disconnect().unwrap();
        backend.connect().expect("reconnect");
        assert_streams(&mut backend, &sdk, -0.5, 0.25);
        backend.disconnect().unwrap();
    });

    process.assert_no_violations();
}

#[test]
fn connects_when_caller_thread_already_has_com() {
    connect_across_threads(CallerApartment::AlreadyInitialized);
}

#[test]
fn connects_when_caller_thread_has_no_com() {
    connect_across_threads(CallerApartment::None);
}

#[test]
fn every_installed_asio_driver_is_discoverable_and_connectable() {
    // Laptops commonly carry several ASIO drivers. The laser interface must not
    // disappear because an earlier driver is still loaded during the scan.
    let process = Process::with_drivers(&[REALTEK, ASIO4ALL, MADIFACE]);
    let (discovery, devices) = scan_on_discovery_thread(process.discovery());

    let mut names = device_names(&devices);
    names.sort();
    assert_eq!(names, [MADIFACE.name, ASIO4ALL.name]);

    let device = take_device(devices, MADIFACE.name);
    let sdk = Arc::clone(&process.sdk);
    run_on_thread("pipeline", CallerApartment::None, move || {
        let mut discovery = discovery;
        let mut backend = connect(&mut discovery, device).expect("connect");
        assert_streams(&mut backend, &sdk, 0.75, 0.75);
        backend.disconnect().unwrap();
    });

    process.assert_no_violations();
}

#[test]
fn background_scans_do_not_disturb_a_live_stream() {
    // Modulaser rescans every few seconds whether or not a DAC is streaming.
    // Scans overlapping connect, streaming, and disconnect must neither unload
    // the live driver nor interleave SDK calls with its setup or teardown.
    let process = Process::with_drivers(&[ASIO4ALL, MADIFACE]);
    let (discovery, devices) = scan_on_discovery_thread(process.discovery());
    let device = take_device(devices, MADIFACE.name);

    let scanning = Arc::new(AtomicBool::new(true));
    let scanner = {
        let (scanning, mut scan_discovery) = (Arc::clone(&scanning), process.discovery());
        thread::Builder::new()
            .name("dac-discovery".into())
            .spawn(move || {
                let mut scans = Vec::new();
                while scanning.load(Ordering::SeqCst) {
                    scans.push(device_names(&scan_discovery.scan()));
                }
                scans
            })
            .unwrap()
    };

    let sdk = Arc::clone(&process.sdk);
    run_on_thread("pipeline", CallerApartment::None, move || {
        let mut discovery = discovery;
        let mut backend = connect(&mut discovery, device).expect("connect");
        for (x, y) in [(0.1, 0.2), (0.3, 0.4), (0.5, 0.6)] {
            assert_streams(&mut backend, &sdk, x, y);
            thread::sleep(Duration::from_millis(30));
        }
        backend.disconnect().unwrap();
        backend.connect().expect("reconnect");
        assert_streams(&mut backend, &sdk, -0.1, -0.2);
        backend.disconnect().unwrap();
    });

    scanning.store(false, Ordering::SeqCst);
    let scans = scanner.join().unwrap();
    assert!(scans.len() > 1, "scanner never overlapped the stream");
    for names in &scans {
        assert!(
            names.iter().any(|n| n == MADIFACE.name),
            "a scan lost the MADIface: {names:?}"
        );
    }
    process.assert_no_violations();
}

#[test]
fn second_stream_on_the_live_asio_driver_is_refused() {
    // Two lasers mapped to the same ASIO device would share one loaded driver;
    // the second open must fail instead of pulling the first stream's buffers.
    let process = Process::with_drivers(&[MADIFACE]);
    let (discovery, first) = scan_on_discovery_thread(process.discovery());
    let (discovery, second) = scan_on_discovery_thread(discovery);
    let (first, second) = (
        take_device(first, MADIFACE.name),
        take_device(second, MADIFACE.name),
    );

    let sdk = Arc::clone(&process.sdk);
    run_on_thread("pipeline", CallerApartment::None, move || {
        let mut discovery = discovery;
        let mut live = connect(&mut discovery, first).expect("connect");
        assert_streams(&mut live, &sdk, 0.5, 0.5);

        let err = connect(&mut discovery, second)
            .err()
            .expect("second stream on the same ASIO driver must be refused");
        assert!(err.to_string().contains("already streaming"), "{err}");

        assert_streams(&mut live, &sdk, -0.5, -0.5);
        live.disconnect().unwrap();
    });

    process.assert_no_violations();
}

#[test]
fn a_hung_driver_call_does_not_wedge_other_callers() {
    // A driver that never returns from stream setup keeps its worker (and the
    // host) busy forever. Everyone else must get an error, not hang with it.
    let process = Process::with_drivers(&[MADIFACE]);
    let engine = Arc::clone(&process.engine);
    let selector = AvbSelector {
        name: MADIFACE.name.to_string(),
        duplicate_index: 0,
    };
    let config = run_on_thread("pipeline", CallerApartment::None, {
        let (engine, selector) = (Arc::clone(&engine), selector.clone());
        move || engine.resolve_stream_config(&selector).unwrap().config
    });

    let release = process.sdk.hang_next_start();
    let (opening_tx, opening) = mpsc::channel();
    let hung_worker = {
        let (engine, selector) = (Arc::clone(&engine), selector.clone());
        thread::spawn(move || {
            // Holds an apartment for the stream's lifetime, as the AVB worker does.
            let _apartment = super::apartment::enter();
            opening_tx.send(()).unwrap();
            let runtime = Arc::new(RuntimeState::new(false, config.sample_rate));
            drop(engine.open_stream(&selector, config, runtime));
        })
    };
    opening.recv().unwrap();
    thread::sleep(SESSION_TIMEOUT / 3);

    let (result_tx, results) = mpsc::channel();
    thread::spawn({
        let (engine, selector) = (Arc::clone(&engine), selector.clone());
        move || {
            let _ = result_tx.send((
                engine.discover().is_err(),
                engine.resolve_stream_config(&selector).is_err(),
            ));
        }
    });
    let (discover_failed, resolve_failed) = results
        .recv_timeout(SESSION_TIMEOUT * 4)
        .expect("discovery/connect blocked behind a hung driver call");
    assert!(discover_failed, "discovery should report the host busy");
    assert!(resolve_failed, "connect should report the host busy");

    drop(release);
    hung_worker.join().unwrap();
    assert!(
        engine.discover().is_ok(),
        "host must recover once the driver returns"
    );
    process.assert_no_violations();
}
