//! LaserDock USB hardware probe — verifies pacing/starvation hypothesis.
//!
//! All samples are blanked (zero color). Run with:
//! `cargo run --release --example usb_probe`

use laser_dac::protocols::lasercube_usb::{DacController, Sample, Stream};
use std::time::{Duration, Instant};

use laser_dac::protocols::lasercube_usb::rusb;
/// `Stream` is now generic over the USB transport; the concrete production type
/// is a stream over a real `rusb` device handle.
type UsbStream = Stream<rusb::DeviceHandle<rusb::Context>>;

fn blank_circle(n: usize) -> Vec<Sample> {
    (0..n)
        .map(|i| {
            let a = (i as f64 / n as f64) * std::f64::consts::TAU;
            let x = (2048.0 + 1600.0 * a.cos()) as u16;
            let y = (2048.0 + 1600.0 * a.sin()) as u16;
            Sample::new(x, y, 0, 0, 0)
        })
        .collect()
}

/// Exact replica of SoftwareDecayEstimator (src/buffer_estimate/software_decay.rs).
struct Est {
    fullness_at_anchor: u64,
    anchor_time: Instant,
}

impl Est {
    fn new() -> Self {
        Self {
            fullness_at_anchor: 0,
            anchor_time: Instant::now(),
        }
    }
    fn read_at(&self, now: Instant, pps: u32) -> u64 {
        let elapsed = now
            .saturating_duration_since(self.anchor_time)
            .as_secs_f64();
        self.fullness_at_anchor
            .saturating_sub((elapsed * pps as f64) as u64)
    }
    fn record_send(&mut self, now: Instant, n: u64, pps: u32) {
        let current = self.read_at(now, pps);
        self.fullness_at_anchor = current.saturating_add(n);
        self.anchor_time = now;
    }
}

fn occupancy(stream: &mut UsbStream, cap: u32) -> u32 {
    cap.saturating_sub(stream.ringbuffer_free_space().unwrap_or(cap))
}

/// Measure device consumption rate: fill, then watch drain slope via 0x8A.
fn measure_drain(stream: &mut UsbStream, cap: u32, label: &str) {
    stream.clear_ringbuffer().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let fill = (cap.saturating_sub(200)).min(6000) as usize;
    let circle = blank_circle(fill);
    stream.send_samples(&circle).unwrap();
    let t0 = Instant::now();
    let occ0 = occupancy(stream, cap);
    println!("  [{label}] filled {fill} samples, occupancy after fill: {occ0}");
    let mut last = occ0;
    let mut last_t = t0;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(25));
        let occ = occupancy(stream, cap);
        let now = Instant::now();
        if occ == 0 || occ == last && now.duration_since(last_t) > Duration::from_millis(500) {
            break;
        }
        if occ != last {
            last = occ;
            last_t = now;
        }
    }
    let occ1 = occupancy(stream, cap);
    let dt = t0.elapsed().as_secs_f64();
    let consumed = occ0.saturating_sub(occ1);
    println!(
        "  [{label}] consumed {consumed} in {:.3}s => {:.0} samples/s (occupancy now {occ1})",
        dt,
        consumed as f64 / dt
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let controller = DacController::new()?;
    let devices = controller.list_devices()?;
    let device = devices
        .into_iter()
        .next()
        .ok_or("no LaserDock USB device found")?;
    let mut stream = Stream::open(device)?;

    // ---- Phase 1: device info ----
    println!("=== Phase 1: device info ===");
    println!("{:#?}", stream.info());
    let cap = stream.info().ringbuffer_capacity;
    let max_rate = stream.info().max_dac_rate;
    let bulk = stream.info().bulk_packet_sample_count;
    println!("capacity={cap} max_rate={max_rate} bulk_packet_sample_count={bulk}");

    // ---- Phase 2: rate set/readback ----
    println!("\n=== Phase 2: rate set/readback (0x82 vs 0x83) ===");
    for rate in [30_000u32, 10_000, max_rate, max_rate + 10_000] {
        match stream.set_rate(rate) {
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(20));
                stream.refresh_device_info()?;
                println!(
                    "  set {rate:>6} -> device reports {}",
                    stream.info().current_rate
                );
            }
            Err(e) => println!("  set {rate:>6} -> REJECTED by device ({e:?})"),
        }
    }
    stream.set_rate(30_000.min(max_rate))?;

    // ---- Phase 3: consumption with output DISABLED ----
    println!("\n=== Phase 3: drain rate, output disabled, rate=30000 ===");
    stream.disable_output()?;
    measure_drain(&mut stream, cap, "output-off");

    // ---- Phase 4: consumption with output ENABLED (blanked samples) ----
    println!("\n=== Phase 4: drain rate, output enabled (blanked), rate=30000 ===");
    stream.enable_output()?;
    measure_drain(&mut stream, cap, "output-on-30k");
    stream.set_rate(10_000)?;
    measure_drain(&mut stream, cap, "output-on-10k");
    stream.set_rate(30_000.min(max_rate))?;

    // ---- Phase 5: USB transaction throughput per write size ----
    println!("\n=== Phase 5: bulk write throughput by chunk size ===");
    for &chunk in &[1usize, 4, 16, 30, 64, 128, 512, 2048] {
        stream.clear_ringbuffer()?;
        std::thread::sleep(Duration::from_millis(50));
        let pts = blank_circle(chunk.max(2));
        let pts = &pts[..chunk];
        // Send up to half the ring so blocking backpressure can't kick in,
        // and cap iterations so tiny chunks don't take forever.
        let total_budget = (cap / 2) as usize;
        let iters = (total_budget / chunk).clamp(1, 400);
        let t0 = Instant::now();
        for _ in 0..iters {
            stream.send_samples(pts)?;
        }
        let dt = t0.elapsed().as_secs_f64();
        let sent = iters * chunk;
        println!(
            "  chunk {chunk:>5}: {iters:>4} writes, {sent:>5} samples in {:>7.1}ms => {:>7.0} samples/s, {:>6.0} µs/write",
            dt * 1e3,
            sent as f64 / dt,
            dt * 1e6 / iters as f64
        );
    }

    // ---- Phase 6: faithful NetworkFifo pacing emulation ----
    println!("\n=== Phase 6: NetworkFifo pacing emulation (20ms target, 30kpps, 12s) ===");
    let pps: u32 = 30_000;
    let target_buffer = Duration::from_millis(20);
    let circle = blank_circle(474);
    let mut cyc = 0usize;
    let mut est = Est::new();
    stream.clear_ringbuffer()?;
    // histogram buckets: 1, 2-4, 5-16, 17-64, 65-256, 257-1024, 1025+
    let mut hist = [0u64; 7];
    let bucket = |n: usize| match n {
        1 => 0,
        2..=4 => 1,
        5..=16 => 2,
        17..=64 => 3,
        65..=256 => 4,
        257..=1024 => 5,
        _ => 6,
    };
    let mut writes = 0u64;
    let mut samples_sent = 0u64;
    let mut min_occ = u32::MAX;
    let mut underruns = 0u32;
    let mut polls = 0u32;
    let t0 = Instant::now();
    let mut next_poll = t0 + Duration::from_millis(100);
    let run = Duration::from_secs(12);
    let mut log_lines: Vec<String> = Vec::new();
    while t0.elapsed() < run {
        let now = Instant::now();
        // -- replica of NetworkFifoAdapter::step --
        let pps_f64 = pps as f64;
        let target_secs = target_buffer.as_secs_f64();
        let target_points = (target_secs * pps_f64) as u64;
        let buffered = est.read_at(now, pps);
        if buffered > target_points {
            let excess = buffered - target_points;
            let sleep_time = Duration::from_secs_f64(excess as f64 / pps_f64);
            // sleep_with_control_check: 2ms slices
            let mut remaining = sleep_time;
            while remaining > Duration::ZERO {
                let slice = remaining.min(Duration::from_millis(2));
                std::thread::sleep(slice);
                remaining = remaining.saturating_sub(slice);
            }
        } else {
            let deficit = (target_secs - buffered as f64 / pps_f64).max(0.0);
            let n = ((deficit * pps_f64).ceil() as usize).min(4096);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(1));
            } else {
                let mut chunkv = Vec::with_capacity(n);
                for _ in 0..n {
                    chunkv.push(circle[cyc % 474]);
                    cyc += 1;
                }
                stream.write_frame(&chunkv, pps)?;
                est.record_send(Instant::now(), n as u64, pps);
                hist[bucket(n)] += 1;
                writes += 1;
                samples_sent += n as u64;
            }
        }
        // -- 10Hz telemetry poll --
        if Instant::now() >= next_poll {
            next_poll += Duration::from_millis(100);
            let occ = occupancy(&mut stream, cap);
            let est_now = est.read_at(Instant::now(), pps);
            min_occ = min_occ.min(occ);
            polls += 1;
            if occ == 0 {
                underruns += 1;
            }
            if polls.is_multiple_of(10) || occ == 0 {
                log_lines.push(format!(
                    "  t={:>5.1}s real_occ={occ:>5} est={est_now:>5}",
                    t0.elapsed().as_secs_f64()
                ));
            }
        }
    }
    for l in &log_lines {
        println!("{l}");
    }
    let dt = t0.elapsed().as_secs_f64();
    println!("  writes={writes} samples={samples_sent} => avg chunk {:.1}, effective {:.0} samples/s (commanded {pps})",
        samples_sent as f64 / writes.max(1) as f64, samples_sent as f64 / dt);
    println!("  chunk histogram [1 | 2-4 | 5-16 | 17-64 | 65-256 | 257-1024 | 1025+] = {hist:?}");
    println!("  polls={polls} min_real_occupancy={min_occ} polls_at_zero={underruns}");

    // ---- Phase 7: reference-style blocking writes (512-sample chunks) ----
    println!("\n=== Phase 7: reference-style blocking stream (512/write, 12s) ===");
    stream.clear_ringbuffer()?;
    let mut cyc = 0usize;
    let mut min_occ = u32::MAX;
    let mut underruns = 0u32;
    let mut polls = 0u32;
    let mut samples_sent = 0u64;
    let t0 = Instant::now();
    let mut next_poll = t0 + Duration::from_millis(100);
    while t0.elapsed() < Duration::from_secs(12) {
        let mut chunkv = Vec::with_capacity(512);
        for _ in 0..512 {
            chunkv.push(circle[cyc % 474]);
            cyc += 1;
        }
        stream.write_frame(&chunkv, pps)?;
        samples_sent += 512;
        if Instant::now() >= next_poll {
            next_poll += Duration::from_millis(100);
            let occ = occupancy(&mut stream, cap);
            min_occ = min_occ.min(occ);
            polls += 1;
            if occ == 0 {
                underruns += 1;
            }
        }
    }
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "  {samples_sent} samples in {dt:.1}s => {:.0} samples/s, polls={polls} min_occ={min_occ} polls_at_zero={underruns}",
        samples_sent as f64 / dt
    );

    stream.stop()?;
    println!("\nDone.");
    Ok(())
}
