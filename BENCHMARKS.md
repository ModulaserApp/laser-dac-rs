# Benchmarks

The benchmark suite measures CPU performance without requiring a physical DAC. `chunk_pipeline` measures one driver iteration's work end to end per DAC and is the target to read first; `presentation`, `resampling`, and `protocol_encoding` isolate the stages underneath it and are the ones to consult when a fused number moves.

It does not measure device latency, transport jitter, underruns, firmware behavior, galvo motion, optical output, or shutter safety. Validate those properties separately with real hardware.

Nor does it measure the driver loop *around* the benched work — the pacing sleeps, buffer-estimator queries, control-message drain, and metrics atomics. That loop is dominated by sleeping and device I/O, so wall-clock over it would measure the pacing rather than the code. `chunk_pipeline` is the highest level at which a timing number still means something.

## Run locally

Run the complete suite:

```bash
cargo bench-all
```

> **A plain `cargo bench` silently does nothing.** Every bench target declares
> `required-features`, and none of those features are enabled by default, so
> cargo skips all four targets and exits 0 without a word. The `bench-all`
> alias in `.cargo/config.toml` supplies the right feature set. There is
> currently no benchmark job in CI; these are local-only measurements.

Run one target with only its required features:

```bash
cargo bench --no-default-features --features testutils --bench presentation
cargo bench --no-default-features --features testutils,avb --bench resampling
cargo bench --no-default-features --features testutils,usb-dacs,network-dacs --bench protocol_encoding
cargo bench --no-default-features --features chunk-pipeline-bench --bench chunk_pipeline
```

Criterion writes local reports under `target/criterion/`.

## Benchmark targets

### `chunk_pipeline`

**Start here.** This is the only target whose unit of work is a thing the driver
actually does: produce a slice, then convert and encode it the way one specific
DAC's backend does *on the driver thread*. Every other target measures a
fragment.

It exists because `presentation` stops at the slice and `protocol_encoding`
starts from one, so adding their numbers misses whatever lives at the seam. That
seam is where this target earned its keep: it caught the Ether Dream backend
converting into a buffer of its own and then copying that into the command
queue's staging buffer, writing every point twice before encoding. The backend
now converts straight into `CommandQueue::data`, which cut the 1799-point chunk
by ~2.9 µs (13.2 µs → 10.3 µs, ~20%) — a cost neither `presentation` nor
`protocol_encoding` could see.

Two caveats on that 20%, so nobody re-derives it the hard way. First, only the
"after" arm is in the tree: the 13.2 µs came from a two-copy fixture that was
removed once the change landed, so the comparison is not reproducible from a
checkout — treat it as a recorded result, not a live one. Second, a plain
`cargo bench-all` cannot resolve an effect this size on a loaded machine;
back-to-back runs of *identical* code have swung ±50% here. It was measured by
interleaving both arms in one process and taking the min of many samples. If you
need to re-establish it, reconstruct the old arm and interleave — a sequential
before/after of two `cargo bench` runs will tell you nothing.

Keep the absolute number in view: 2.9 µs against a 59.97 ms chunk cadence is
~0.005% of the driver's budget. The copy was worth deleting because it was free
to delete, not because it was ever a bottleneck.

Note what that number does and does not rest on. `DriverThreadEncoder`
*reproduces* each backend's buffer discipline rather than calling the backend, so
it tracks production only as long as someone keeps the two in step: the fixture
is evidence about the model, and the model is only as good as its fidelity to
`EtherDreamBackend::try_write_points`. When you change a backend's buffering,
change its fixture in the same commit, or this target will confidently measure a
DAC that no longer exists.

One thing still lives only here:

- **IDN and LaserCube-network regrow their conversion buffer every chunk.** Both
  backends hand the buffer to a worker thread by `mem::take`, so the next write
  allocates. This is deliberate and correct — the worker owns the chunk — but
  `ProtocolEncoderBenchmark` retains warm buffers and cannot see it.

Cases run at each DAC's real `max_points_per_chunk`, because that is what the
FIFO adapter clamps its request to: IDN 179, Ether Dream 1799, LaserCube-network
80 (Ethernet / Wi-Fi client) and 140 (Wi-Fi server), LaserCube USB 4096, Helios
4095 of frame capacity. Ether Dream also runs at its ~5 ms quantum floor (150
points at 30 kHz), the other end of its request range.

**Read these per chunk. No `Throughput` is set on purpose** — a chunk is the unit
that has to fit inside the driver's cadence.

Where the driver thread encodes (Helios, Ether Dream, LaserCube USB) the fixture
reports bytes. Where encoding belongs to a worker thread (IDN, LaserCube-network)
it reports converted points, and the encode half is covered by
`protocol_encoding` instead. Fusing those two halves into one number would
measure something no single thread does.

FIFO cases queue no frame per iteration: chunks are pulled far more often than
content arrives, and the frame loops so each wrap drives a real transition. The
Helios case queues one per iteration, since a frame-swap DAC repeats the frame in
hardware and is only written when content changes.

Excluded because they are I/O or scheduling rather than CPU: the adapter's pacing
sleeps, buffer-estimator queries (`Instant::now`), the mpsc control drain, the
metrics atomics, and the device write.

#### What it measured

On an M-series laptop at 30 kHz, against the wall-clock each chunk has to cover:

| Case | Points | Per chunk | Chunk covers | Budget used |
|---|---|---|---|---|
| `fifo/lasercube_network` | 80 | 147 ns | 2.67 ms | 0.006% |
| `fifo/lasercube_network_wifi` | 140 | 230 ns | 4.67 ms | 0.005% |
| `fifo/idn` | 179 | 353 ns | 5.97 ms | 0.006% |
| `fifo/ether_dream_quantum` | 150 | 867 ns | 5.00 ms | 0.017% |
| `fifo/ether_dream` | 1799 | 10.3 µs | 59.97 ms | 0.017% |
| `fifo/lasercube_usb` | 4096 | 9.83 µs | 136.5 ms | 0.007% |
| `frame_swap/helios` | 500 → 4095 cap | 3.42 µs | content rate | negligible |

Two conclusions worth keeping:

**Driver-thread CPU is not a bottleneck on any DAC, by three to four orders of
magnitude.** If a stream underruns, look at scheduling, transport, or the device
— not at this code. Treat these numbers as a regression tripwire, not a budget.

**Per-chunk cost is essentially linear in points; fixed cost is unmeasurable.**
Ether Dream's two sizes fit a line of ~5.7 ns/point with an intercept
indistinguishable from zero (under ~150 ns). So the suite's per-point throughput
framing is sound, and the small-chunk DACs are not paying a hidden fixed toll.
IDN's ~2 ns/point against Ether Dream's ~5.7 is the driver/worker split doing
exactly what it should: IDN's driver thread only converts, while Ether Dream's
also encodes to wire bytes.

### `presentation`

- FIFO copying with coalesced seams
- FIFO self-loop transitions using `default_transition`
- pending-frame promotion
- frame-swap composition at 500 and 4095 authored points
- the FIFO slice pipeline with 0, 5, and 32 points of color delay
- the frame-swap slice pipeline at 500 and 4095 points with 0 and 5 points of color delay
- short and full-scale default transition construction

The transition cases include the allocation performed by `default_transition`. The coalesced FIFO case isolates warmed copying and frame lifecycle overhead.

Both slice-pipeline families run the production `default_transition`, so composition, blanking, and color delay are all represented. The `presentation/frame_swap` group measures only `PresentationEngine::compose_hardware_frame` — the engine half of what a frame-swap DAC (Helios, `OutputModel::UsbFrameSwap`) runs. The pipeline half is `slice_pipeline/frame_swap_color_delay`, and it is not a rounding error: at 4095 points, composition is ~2.1 µs and a 5-point color delay adds ~3 µs on top of it, so the engine-only number accounts for well under half the real per-frame cost.

The pipeline fixtures install no output filter. Filters are user-supplied, so their dispatch cost is not measured here. `output_filter: None` is also the default, and what the fixtures skip is one `Option` check and one vtable call **per chunk** — the trait hands the filter the whole slice, so dispatch does not scale with points.

The FIFO cases here pull 4096-point chunks, which is not any FIFO DAC's real chunk size except LaserCube USB — the adapter clamps its request to `max_points_per_chunk`, which is 179 for IDN and 1799 for Ether Dream. That is deliberate: this group isolates per-point presentation cost, where a large chunk gives the cleanest signal. For real chunk sizes joined to real conversion and encoding, read `chunk_pipeline` instead.

`pending_frame_promotion` and `frame_change` queue a frame on every call. That is a deliberate worst case for isolating swap cost — real content arrives at frame rate while chunks are pulled far more often.

### `resampling`

`avb_stream` is the AVB write path: `LaserPoint` conversion plus a 6-channel (XYRGBI) Catmull-Rom resample, at 30 kHz→48 kHz, 30 kHz→96 kHz, and 60 kHz→48 kHz. The last case runs PPS above the device rate, which the backend handles by decimating. Phase and history carry between measured chunks, matching backend use.

`oscilloscope_stereo` covers the oscilloscope backend's 2-channel (XY only) resample and is only built with the `oscilloscope` feature. Do not read it as an AVB number: at 30 kHz→48 kHz the 6-channel AVB path costs roughly twice as much per input point.

The AVB backend emits into a lock-free `ArrayQueue` drained by the audio callback. The fixture emits into a `Vec`, so queue contention and the callback side are not represented.

### `protocol_encoding`

Each protocol case converts `LaserPoint` values and writes the wire representation into retained buffers:

- Helios
- Ether Dream
- IDN XYRGBI
- LaserCube network
- LaserCube USB

Fixture buffers are reserved before measurement so the timed path represents warmed, reusable encoding buffers. Use an allocation profiler when a timing change suggests that allocations entered this path.

## Input content

Frame benchmarks use `common::artwork_points`: three disjoint open polygons separated by blanked jumps. The properties that matter are that shapes are far apart (so jumps are long and blanked), outlines have straight edges and corners, blanked points arrive in runs, and the path ends far from where it starts.

That last property is load-bearing. `common::laser_points` draws a *closed* circle, so a frame's last→first seam is a near-zero-distance transition — switching the FIFO self-loop case to artwork raised it by ~50%, and the coalesced-to-real-transition change in the slice pipeline more than doubled it. A best-case seam quietly understates every transition-sensitive number, so prefer `artwork_points` for anything that crosses a frame boundary. `laser_points` remains useful for isolating per-point cost where geometry is irrelevant.

## Measurement rules

- Inputs are deterministic.
- Setup and input generation happen outside measured closures.
- Inputs and outputs pass through `black_box`.
- Every case checks output size before measurement.
- Throughput counts **input** points, since that is what a backend is handed per call. Upsampling cases therefore produce more output per counted element, and a lower reported throughput does not mean less work was done.
- Benchmark names are historical identifiers. Rename one only when its workload contract changes.
- Keep sleeps, sockets, device discovery, logging, and GUI work out of blocking benchmarks.
- Benchmark the type the production path actually uses. A fixture that resamples a convenient stand-in type measures a workload nothing ships.

The feature-gated interface in `src/benchmark_support.rs` exposes benchmark fixtures without adding those internals to the normal crate interface.

## Compare a local baseline

Criterion baselines let you compare a change on the same machine:

```bash
# Run this before making the change.
cargo bench --no-default-features \
  --features testutils,usb-dacs,network-dacs,avb,chunk-pipeline-bench \
  -- --save-baseline main

# Run this after making the change.
cargo bench --no-default-features \
  --features testutils,usb-dacs,network-dacs,avb,chunk-pipeline-bench \
  -- --baseline main
```

The saved baseline lives under `target/criterion/`. Do not run `cargo clean` between the two commands.

Use the same machine, power mode, Rust toolchain, feature set, and lockfile for both runs. Stop other CPU-heavy work first. Re-run surprising results before treating them as regressions. Local wall-clock baselines are useful for development, but they do not provide shared historical tracking or a reliable merge gate.

When intentionally updating the Rust compiler, benchmark inputs, dependencies, or release profile, treat the resulting baseline shift separately from application-code changes.
