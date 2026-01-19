# Example Streaming Pipeline  Architecture

This document describes an example high-level streaming pipeline for professional laser show software built on top of `laser_dac`'s streaming API.

The goal is to make **control flow**, **data flow**, and **where “frames” still exist** unambiguous, while keeping implementation details out.

## Mental model

- A DAC stream is a **continuous point signal** at a fixed PPS.
- `laser_dac` owns **pacing/backpressure** and provides a **timebase**.
- Your application owns **content**, **composition**, **zones**, and **safety**.

The application does not “push frames on a timer”. Instead, it implements a function that can answer:

> “For this `ChunkRequest`, what are the `n_points` I should output?”

## Control flow (who asks whom)

`laser_dac::Stream` decides when it needs more data:

- It emits a `ChunkRequest { start, pps, n_points, scheduled_ahead_points, device_queued_points }`.
- Your show engine produces exactly `n_points` and returns them.
- The stream writes the chunk to the backend/DAC.

This pull-style control flow is the key difference versus traditional "push frames on a timer" APIs.

```
DAC needs points
  -> Stream creates ChunkRequest
    -> your producer(req) renders chunk via zones/mixer/sources
      -> Stream writes chunk -> backend -> DAC
```

Operationally this happens either via:

- **Blocking mode**: `next_request()` → render → `write()`
- **Callback mode**: `run(producer, on_error)` calls your producer when needed; returning `None` ends the stream gracefully

## End-to-end topology

Think of the show engine as one shared graph that fans out into per-zone/per-DAC output pipelines:

```
Sources / Clips  --->  Mixer / Timeline  --->  Zone Router  --->  Zone Output Pipelines
                                                           \-->  (one per zone/DAC)
```

Preview is not a separate pipeline; it’s a tap on this graph (often “post-zone, post-safety”).

## Sources / Clips: frame-shaped vs continuous

All sources must ultimately answer chunk requests, but sources differ in their *native* shape:

### Frame-shaped sources (example: NDI @ 30 fps)

- The source produces updates at ~30 Hz (frames).
- The DAC stream still needs chunks at (say) 30k–100k PPS.

So the source keeps its native “frame” representation upstream (e.g. a “latest frame” buffer), and on each `ChunkRequest` it:

- reads the latest available frame
- converts it into points suitable for this chunk
- ensures the final output is exactly `req.n_points`

In other words: **frames still exist, but earlier in the pipeline**.

### Continuous sources (example: XY oscilloscope)

- There is no meaningful “frame boundary”.
- The natural definition is: “given a time window, generate points for that window.”

With streaming, the time window is explicit:

- start time: `req.start`
- duration: `req.n_points / req.pps`

So an oscilloscope source typically:

- uses `req.start` + `req.n_points` to pick the correct audio window from a ring buffer
- deterministically generates exactly `req.n_points`

Chunk boundaries exist only because the stream needs chunks, not because the source is frame-based.

## Zones (routing + per-zone parameters)

A zone is the unit that lets one show drive multiple DACs differently. Each zone typically has:

- a target DAC (or “none”, preview-only)
- geometry/mapping settings (how content is placed into that zone)
- masking/blind settings (where output should be suppressed)
- safety parameters (because projection size/geometry changes scan stress)

The important architectural point is: **zones fan out**. One show can render different outputs per zone, and each zone can run at different PPS and constraints because it targets a specific DAC stream.

## Per-zone output pipeline (per chunk)

Each zone has a pipeline that runs once per `ChunkRequest`. The exact stages can vary, but the typical ordering is:

1. **Render content for the request**
   - Use `req.start`/`req.pps`/`req.n_points` as the time window.
   - Pull from sources via the mixer (NDI “latest frame”, oscilloscope via audio window, etc.).

2. **Apply zone transforms**
   - Apply zone placement/scaling/mapping.
   - Apply masking/blinding (strategy is app-defined).

3. **Budgeted composition (travel / settle / blank lead-in/out)**
   - Reserve part of the fixed `req.n_points` budget for scanner/laser behavior such as blank travel, pre/post settle dwell, and blank lead-in/out for modulation latency.
   - Express these budgets in time (e.g. milliseconds) and convert to points via PPS (`points = pps * seconds`), then clamp to available budget so behavior scales predictably as PPS changes.
   - These are not “extra points”; they consume points from the same fixed chunk budget.

4. **Point-count normalization (exact length)**
   - Produce **exactly `req.n_points`** points (hard contract).
   - If upstream stages produced variable-length geometry/points, this is where you fit them into the fixed budget (resample/decimate/repeat) deterministically.

5. **Final motion limiting (length-preserving)**
   - Enforce max step/velocity/acceleration limits at the current PPS without changing point count (e.g. a slew/Δ clamp).
   - This is the last line of defense against “big motion glitches”, and it prevents normalization/decimation from reintroducing unsafe jumps.

**Non-negotiable (streaming contract):** any safety kernel that conceptually “inserts points” must be implemented in a fixed-count way (budgeted), or via an `oversample → normalize-to-exact-count → final length-preserving limiter` pattern. The producer must always return exactly `req.n_points`.

6. **Beam dwell (“one-beam”) protection**
   - Detect near-static output at nonzero intensity (e.g. oscilloscope plateaus/extremes).
   - Operate late to catch “almost static after transforms / limiting”.
   - If a dwell threshold is exceeded, override with safe output (blank/park) and optionally escalate to disarm.

7. **Output gate (E-stop / arm state)**
   - If disarmed, output safe points regardless of what upstream produced.

8. **Submit**
   - Return the points to `laser_dac` for this request.

### Deterministic degrade rules (when the budget is tight)

Sometimes the “ideal” output (detail + travel + settle + safety) does not fit inside `req.n_points`. Make the behavior explicit and deterministic:

1. Never violate safety constraints (motion limits, dwell limits, disarm).
2. Prefer continuity (no big jumps) over fidelity.
3. Reduce detail (simplify/decimate) before dropping safety overhead.
4. If you must fail-safe, end the chunk in a safe state (blank/park) while keeping continuity.

## Multi-DAC output (different types, different scale)

This architecture supports multiple DACs naturally:

- Each DAC gets its own `StreamConfig` (PPS, queue target) and its own chunk producer.
- Each zone pipeline can be parameterized differently (e.g. different safety limits due to projection scale).
- The mixer/sources can be shared, but the final rendering is per zone/per request.

If you later need phase-aligned multi-projector rigs, add a shared show epoch so all streams interpret `req.start` in the same show-time coordinate.

## Reliability and safety supervision

Two complementary mechanisms:

- **E-stop / disarm (out-of-band)**: must work even if the producer/rendering is stalled.
- **Watchdog (separate thread)**: independently monitors liveness and triggers disarm (and reports diagnostics).

### Make E-stop effective even if rendering is busy

There are two places you can “gate” output, and they behave differently under stalls:

- **In-producer gating** (e.g. the producer checks a shared flag and returns blank/park points):
  - Good as a last-mile safety layer.
  - Not sufficient on its own, because it only takes effect once the producer is called again.

- **Out-of-band stream control** (preferred for pro rigs):
  - A thread-safe control path that can be triggered by a watchdog/UI thread even when the producer is stuck.
  - Conceptually: “disarm this stream now” (and optionally stop/flush output depending on backend).

Practical consequence: keep stream queue targets bounded (`target_queue_points`) so worst-case “already scheduled output” after an E-stop is limited, even if device-level flushing isn’t perfect.

## Non-negotiable contracts with `laser_dac`

- For every `ChunkRequest`, the producer must return **exactly `n_points`** (or `None` in callback mode to end the stream).
- `scheduled_ahead_points` is the primary signal for "how far ahead am I?"; use it for UI/telemetry and to detect runaway/underrun risk. `device_queued_points` provides a device-level estimate when available.
- In blocking mode, `next_request()` is the pacing mechanism; avoid adding your own "sleep loop" on top.
- For professional safety expectations, treat "disarm" as an **out-of-band control path** via `StreamControl`, not only as "the producer starts outputting blanks".
