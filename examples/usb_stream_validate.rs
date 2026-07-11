//! End-to-end hardware validation of the LaserDock USB (BlockingFifo) fix.
//!
//! Drives the REAL public streaming API (`Dac::start_stream` + `Stream::run`)
//! against an attached LaserDock, so it exercises the production
//! `BlockingFifoAdapter` code path — not a probe replica.
//!
//! All points are blanked (zero color): safe to run with the laser physically
//! connected or not. Validates:
//!   1. Sustained effective throughput ≈ commanded PPS (starvation would show
//!      as a rate well below 30k).
//!   2. Disarm mid-stream for ~2s then re-arm resumes cleanly without wedging
//!      the blocking write endpoint.
//!
//! Run with: `cargo run --release --example usb_stream_validate`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use laser_dac::{list_devices, open_device, DacType, LaserPoint, Result, StreamConfig};

const PPS: u32 = 30_000;
const CIRCLE_POINTS: usize = 474;
const RUN_SECS: u64 = 16;
const DISARM_AT: Duration = Duration::from_secs(6);
const REARM_AT: Duration = Duration::from_secs(8);

fn blank_circle(n: usize) -> Vec<LaserPoint> {
    (0..n)
        .map(|i| {
            let a = (i as f64 / n as f64) * std::f64::consts::TAU;
            LaserPoint::blanked(0.8 * a.cos() as f32, 0.8 * a.sin() as f32)
        })
        .collect()
}

fn main() -> Result<()> {
    env_logger::init();

    println!("Scanning for DACs...");
    let devices = list_devices()?;
    let info = devices
        .iter()
        .find(|d| d.kind == DacType::LaserCubeUsb)
        .ok_or_else(|| laser_dac::Error::disconnected("no LaserCube USB (LaserDock) found"))?;
    println!("  Using: {} ({})", info.name, info.kind);

    let device = open_device(&info.id)?;
    let (stream, info) = device.start_stream(StreamConfig::new(PPS))?;
    println!(
        "  output_model={:?} pps={PPS} chunk-produced-by-adapter\n",
        info.caps.output_model
    );

    let control = stream.control();
    let points_sent = Arc::new(AtomicU64::new(0));

    // Producer: fill exactly what the adapter asks for (the fixed BlockingFifo
    // chunk) with the blanked circle, and count delivered points.
    let circle = blank_circle(CIRCLE_POINTS);
    let counter = Arc::clone(&points_sent);
    let mut cursor = 0usize;
    let producer = move |req: &laser_dac::ChunkRequest, buf: &mut [LaserPoint]| {
        let n = req.target_points.min(buf.len());
        for slot in buf.iter_mut().take(n) {
            *slot = circle[cursor % CIRCLE_POINTS];
            cursor += 1;
        }
        counter.fetch_add(n as u64, Ordering::Relaxed);
        laser_dac::ChunkResult::Filled(n)
    };

    // Control/telemetry thread: arm, sample throughput every 500ms, disarm/
    // re-arm on schedule, then stop.
    let ctl = control.clone();
    let counter = Arc::clone(&points_sent);
    let telemetry = std::thread::spawn(move || {
        ctl.arm().unwrap();
        let start = Instant::now();
        let mut last = 0u64;
        let mut last_t = start;
        let mut disarmed = false;
        let mut rearmed = false;
        let mut min_armed_rate = f64::INFINITY;
        println!("  t(s)  interval_rate(samples/s)  state");
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let elapsed = start.elapsed();

            if !disarmed && elapsed >= DISARM_AT {
                ctl.disarm().unwrap();
                disarmed = true;
                println!("  --- disarm() at t={:.1}s ---", elapsed.as_secs_f64());
            }
            if !rearmed && elapsed >= REARM_AT {
                ctl.arm().unwrap();
                rearmed = true;
                println!("  --- arm() at t={:.1}s ---", elapsed.as_secs_f64());
            }

            let now = Instant::now();
            let count = counter.load(Ordering::Relaxed);
            let dt = now.duration_since(last_t).as_secs_f64();
            let rate = (count - last) as f64 / dt;
            let armed_now = !disarmed || rearmed;
            let state = if armed_now { "armed" } else { "DISARMED" };
            println!(
                "  {:>4.1}  {:>22.0}  {}",
                elapsed.as_secs_f64(),
                rate,
                state
            );
            // Only fold intervals fully inside an armed window into the min.
            if armed_now
                && elapsed > Duration::from_secs(1)
                && !(rearmed && elapsed < REARM_AT + Duration::from_millis(600))
            {
                min_armed_rate = min_armed_rate.min(rate);
            }
            last = count;
            last_t = now;

            if elapsed >= Duration::from_secs(RUN_SECS) {
                break;
            }
        }
        ctl.stop().unwrap();
        min_armed_rate
    });

    let run_start = Instant::now();
    let exit = stream.run(producer, |err| eprintln!("  stream error: {err}"))?;
    let run_dt = run_start.elapsed().as_secs_f64();
    let min_armed_rate = telemetry.join().unwrap();

    let total = points_sent.load(Ordering::Relaxed);
    // Delivered during ~14s of armed time (16s total minus ~2s disarmed).
    let armed_secs = run_dt - (REARM_AT - DISARM_AT).as_secs_f64();
    println!("\n=== Summary ===");
    println!("  run exit: {exit:?} (thread did not wedge)");
    println!("  total points delivered: {total}");
    println!(
        "  mean throughput over armed time (~{armed_secs:.1}s): {:.0} samples/s (commanded {PPS})",
        total as f64 / armed_secs
    );
    println!("  min per-interval armed rate: {min_armed_rate:.0} samples/s");
    let ok = min_armed_rate > 27_000.0;
    println!(
        "  VERDICT: {} (armed throughput within ~10% of commanded, no wedge)",
        if ok { "PASS" } else { "FAIL" }
    );
    Ok(())
}
