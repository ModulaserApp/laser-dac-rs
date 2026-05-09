# Scheduler unification + BufferEstimator

Status: proposed
Tracking deepening opportunities #1 + #2 from the architecture review.

## Goal

Collapse the two parallel scheduler loops in this crate into one, and concentrate buffer-fullness estimation into a single, protocol-owned authority.

Today:

- `presentation::FrameSession::run_loop` (`src/presentation/session.rs`) cleanly delegates per-[OutputModel](#outputmodel) pacing to a `OutputModelAdapter` family (`src/presentation/output_model/`).
- `stream::Stream::run` (`src/stream/mod.rs:817–967`) re-implements every piece of that orchestration inline: `scheduled_ahead` decay, target-buffer sleep, `sleep_until_with_control_check` for `UdpTimed`, `process_control_messages`, `handle_reconnect`, `drain_and_blank`.
- Buffer estimation is split: each [Backend](#backend) optionally implements `queued_points()`, and the scheduler folds it with its own software decay via `scheduler::conservative_buffered_points`. `lasercube_wifi/dac/buffer_estimator.rs` (489 lines) is a sophisticated dual-track estimator that nobody else can use; `ether_dream::backend::decay_fullness` is a one-off; `src/scheduler.rs` is a third version.

After this refactor:

- One driver loop, one [OutputModelAdapter](#outputmodel) family, one place where pacing semantics live.
- Each FIFO [Backend](#backend) owns a `BufferEstimator` strategy. `BackendKind::queued_points()` retires; `src/scheduler.rs` retires.
- `Stream::run` and `FrameSession::run_loop` become thin façades over the same loop, parameterised by a `ContentSource` trait.

## Design

### The two-axis matrix

The two intake shapes (Frame slot vs callback) and the three [OutputModel](#outputmodel) variants are orthogonal — except that whole-atomic-frame upload (`UsbFrameSwap`) is intrinsically frame-shaped:

|                  | UsbFrameSwap | NetworkFifo | UdpTimed |
|------------------|:---:|:---:|:---:|
| Frame intake     | ✓ today | ✓ today | ✓ today |
| Chunk intake     | ✗ rejected | ✓ today | ✓ today |

Frame intake comes from `SlicePipeline` (a [Frame](#frame) slot composed via `PresentationEngine` + `transition_fn`). Chunk intake comes from a user callback. `Dac::start_stream` already rejects `UsbFrameSwap` backends, and that stays.

### Seam 1 — ContentSource

A pair of traits in a new `src/presentation/content_source.rs` module. The post-write hook is split from cache invalidation so the contract is loud, not a coincidence-of-call-order:

```rust
pub(crate) trait FifoContentSource: Send {
    /// Produce up to `target_points` points for the next chunk. Decorations
    /// (idle policy, startup blank, color delay, output filter) are applied
    /// inside the source. Returns `&[]` when no work is currently available.
    fn produce_chunk(&mut self, target_points: usize, pps: u32, is_armed: bool) -> &[LaserPoint];

    /// The previously-produced slice if not yet written (retain-on-WouldBlock).
    /// Returns `None` after `commit_written` or `discard_cached`.
    fn cached_slice(&self) -> Option<&[LaserPoint]>;

    /// Adapter MUST call exactly once after a successful `try_write` of `n`
    /// points from the cached slice. Updates derived state: ChunkProducer
    /// records last_chunk for RepeatLast, advances current_instant, bumps
    /// stats. SlicePipeline only clears the cache.
    fn commit_written(&mut self, n: usize, is_armed: bool);

    /// Drop the cache without recording a write. For explicit reset paths
    /// only (reconnect, stop). NOT a synonym for commit_written.
    fn discard_cached(&mut self);

    fn reserve_buf(&mut self, n: usize);

    /// Reset derived state on reconnect. Implementations that have a logical
    /// frame to replay (SlicePipeline) re-prime it here.
    fn on_reconnect(&mut self, info: &DacInfo);

    /// Producer requested graceful shutdown; the adapter should drain and the
    /// driver should exit `RunExit::ProducerEnded`.
    fn is_ended(&self) -> bool;
}

pub(crate) trait FrameContentSource: Send {
    /// Produce a complete frame for atomic upload. Returns `&[]` when no
    /// frame is currently available.
    fn produce_frame(&mut self, pps: u32, is_armed: bool) -> &[LaserPoint];
    fn cached_slice(&self) -> Option<&[LaserPoint]>;
    fn commit_written(&mut self, n: usize, is_armed: bool);
    fn discard_cached(&mut self);
    fn on_reconnect(&mut self, info: &DacInfo);
}
```

Implementations:

- `SlicePipeline` (existing) implements **both**. `produce_chunk` is today's `produce_fifo_chunk`; `produce_frame` is today's `produce_frame_swap`. `commit_written` clears the cache (frame composition is stateless across writes; no per-write derived state to advance). `discard_cached` is the same body. `on_reconnect` re-primes `last_frame`, resets color delay, fires `OutputResetReason::Reconnect` on the output filter.
- `ChunkProducer` (new, in stream module) wraps the user `producer` callback + idle policy + startup blank + color delay + `RepeatLast` cache. Implements `FifoContentSource` only. `produce_chunk` invokes the callback, applies decorations, returns the buffer. `commit_written(n, is_armed)` does what today's `Stream::record_write` does: copies `last_chunk[..n]`, advances `current_instant`, bumps stats — **only on success**. `discard_cached` only invalidates. `ChunkResult::Starved` and `Filled(0)` collapse internally to "idle policy fill" — the adapter never has to know. `ChunkResult::End` flips `is_ended()` and the next call returns `&[]`.

The existing `OutputModelAdapter` family changes its parameter from `&mut SlicePipeline` to `&mut dyn FifoContentSource` (`NetworkFifoAdapter`, `UdpTimedAdapter`) or `&mut dyn FrameContentSource` (`UsbFrameSwapAdapter`). Adapters that today call `pipeline.invalidate()` after `Ok(WriteOutcome::Written)` switch to `source.commit_written(n, ctx.is_armed)`. No adapter calls `discard_cached` in the steady-state path; only the driver's reconnect / stop branches do.

### Seam 2 — BufferEstimator

A read-only trait in a new `src/buffer_estimate/` module. The trait is one method by design: callers (the adapter) ask one question, and all event-recording is private to the concrete strategy and driven by the [Backend](#backend) from inside its protocol code.

```rust
pub trait BufferEstimator: Send {
    /// Best estimate of how many points are still queued in the device at
    /// `now`, given the current point rate. Strategies that don't need pps
    /// (e.g. RuntimeAuthorityEstimator) ignore it.
    fn estimated_fullness(&self, now: Instant, pps: u32) -> u64;
}
```

Pps flows through the read method rather than being held as estimator state. The adapter already reads `pps = ctx.control.pps()` every step (see `network_fifo.rs:35`, `udp_timed.rs:56`) so passing it through is free, and it keeps the trait free of mutation methods that would conflict with the `&dyn` read-only seam.

When a strategy DOES need to anchor its decay-rate on pps changes (LC-WiFi today calls `set_point_rate` to reset its rate), the backend forwards from inside `try_write_points(pps, ...)` to its concrete estimator's `set_point_rate(pps)`. The backend is the natural trigger point because pps flows in there.

Strategies, each as a small struct in `src/buffer_estimate/` with **concrete** event-recording methods (NOT on the trait — backends call them directly because they hold the concrete type):

- `SoftwareDecayEstimator` — no telemetry. **Anchor-based**: holds `(fullness_at_anchor: u64, anchor_time: Instant)`. `estimated_fullness(now, pps)` computes `fullness_at_anchor.saturating_sub((now - anchor_time).as_secs_f64() * pps as f64) as u64` — a pure read from a fixed anchor. Concrete event method: `record_send(&mut self, now: Instant, n: u64, pps: u32)` first reads the current fullness via the read method, then rebases `fullness_at_anchor = current + n; anchor_time = now`. Concrete `reset()`. Today's `fractional_consumed` field disappears — it only existed because the incremental-update pattern in `advance_scheduled_ahead` accumulated truncation across calls; anchor reads compute from a fixed anchor each time, so the elapsed-since-anchor measurement carries the precision. No interior mutability and no per-read state advance.
- `StatusDecayEstimator` — periodic authoritative reports. Anchor-based on `(last_status_time, last_status_fullness)`; reads decay from the most recent status. Concrete: `record_status(now, fullness)` (sets the anchor), `record_send(now, n_points)` (rebases like SoftwareDecay), `reset()`. This is today's Ether Dream `decay_fullness`, generalised.
- `DualTrackAckEstimator` — UDP send + ACK correlation. Already anchor-shaped (today's `lasercube_wifi/dac/buffer_estimator.rs:69–86`). Maintains sent-track `(last_data_sent_time, last_data_sent_buffer_size)` and ack-track `(last_ack_time, last_reported_buffer_fullness)`; `estimated_fullness` reports `max(sent, ack)` after individual decay. Concrete: `record_send(now, message_number, n_points)`, `record_ack(now, message_number, fullness)`, `set_point_rate(pps)`, `reset()`.
- `RuntimeAuthorityEstimator` — backed by an external runtime that already knows queue depth. Thin wrapper that forwards `estimated_fullness` to `runtime.queued_points()`. No event hooks needed — the runtime sees sends through its own channel. Concrete: just construction.

Each FIFO [Backend](#backend) holds the concrete strategy as a **named field**, not a `Box<dyn BufferEstimator>`. Boxing would block the backend from calling protocol-specific methods (`record_status`, `record_ack`, `set_point_rate`). For example:

```rust
pub struct EtherDreamBackend {
    // ...
    estimator: StatusDecayEstimator,  // concrete, not Box<dyn>
}
```

The `FifoBackend` trait gains a read-only getter:

```rust
fn estimator(&self) -> &dyn BufferEstimator;
```

Note `&self`, not `&mut self`. Adapters cannot mutate; mutation is the backend's job. The backend feeds events to its concrete estimator from inside `try_write_points` (records sends — including any partial-accept counts the backend knows about), from its status-response handler (records reports), from its ACK handler (records ACKs), and from `connect()` / reconnect (resets).

This also fixes a subtle correctness bug today: `ether_dream/backend.rs:165` does `count = points.len().min(available)` — it sends fewer points than requested but returns `Written`. With backend-internal recording, the estimator sees `count`, not `points.len()`. An adapter-driven `record_send(points.len())` would systematically over-count.

`BackendKind::queued_points()` retires (in phase 2 — phase 1 leaves it intact, see phasing below). `NetworkFifoAdapter::step` calls `backend.estimator().estimated_fullness(now, ctx.pps)` directly. The adapter's `scheduled_ahead`, `fractional_consumed`, `last_iteration` fields all go away — that bookkeeping has moved into whichever estimator strategy the backend holds.

### Per-protocol mapping

| Protocol         | OutputModel    | Estimator strategy       | Notes |
|---|---|---|---|
| Helios           | UsbFrameSwap   | none (frame-swap)        | No FIFO interaction. |
| Ether Dream      | NetworkFifo    | StatusDecay              | Status responses → `record_status(now, fullness)`. |
| IDN              | NetworkFifo / UdpTimed | SoftwareDecay    | No authoritative report today. Future upgrade if IDN packet timestamps prove useful. |
| LaserCube USB    | NetworkFifo    | SoftwareDecay            | `try_write_points` → `record_send`; no telemetry today. |
| LaserCube WiFi   | UdpTimed       | DualTrackAck             | Existing 489-line estimator becomes the canonical strategy. |
| AVB              | UdpTimed       | RuntimeAuthority         | Audio runtime already authoritative. |
| Oscilloscope     | UdpTimed       | RuntimeAuthority         | Audio runtime already authoritative. |

### Driver loop unification

The body of `presentation::session::FrameSession::run_loop` becomes the only driver loop. It moves to its natural home — `src/presentation/driver.rs` — and is parameterised by:

- a `BackendKind`,
- a `ContentSourceKind` enum (`Fifo(Box<dyn FifoContentSource>)` / `Frame(Box<dyn FrameContentSource>)`),
- a `StreamControl` + control-message receiver,
- a `SessionMetrics` (renamed from `FrameSessionMetrics`),
- an optional `ReconnectPolicy` and a `ReconnectValidator` closure (replaces the two reconnect implementations' validators),
- an `error_sink: Box<dyn FnMut(Error) + Send>` for non-fatal write errors that warrant diagnostic but don't disconnect.

`LoopCtx` exposes `error_sink` so adapters can call `ctx.error_sink(e)` for the non-fatal error paths (today's `on_error(e)` calls inside `Stream::write_fill_points`). Frame-mode façade passes a no-op closure (preserving today's silent behaviour); Stream-mode façade passes the user's `on_error`. No diagnostic is lost across the unification.

**End-of-stream exit.** After every `adapter.step(ctx)`, the driver checks `source.is_ended()`. If true, it calls `adapter.drain_and_blank(ctx, drain_timeout)` and returns `Ok(RunExit::ProducerEnded)`. Frame source's `is_ended()` is always false (Frame intake never ends from the source side); Stream source's `is_ended()` flips when the user callback returns `ChunkResult::End`. Adapter doesn't need to know about End semantics — it just keeps stepping until the driver tells it to drain.

`Stream::run` becomes:

1. Build a `ChunkProducer` from `producer` + config decorations.
2. Pick the adapter via `OutputModelAdapter::for_backend(&backend)`.
3. Call `driver::run(backend, ContentSourceKind::Fifo(Box::new(chunk_producer)), control, on_error, …)`.

`FrameSession::run_loop` becomes:

1. Build a `SlicePipeline` from the engine + filter.
2. Pick the adapter (existing).
3. Call `driver::run(backend, ContentSourceKind::Fifo|Frame(...), control, /* error_sink: */ Box::new(|_| {}), …)`.

The driver owns: `process_control_messages`, the shutter transition handler, the reconnect-with-retry loop, the loop-activity metric pings, the `StepOutcome` dispatch, and the post-step `is_ended()` check. Drain-on-end becomes `OutputModelAdapter::drain_and_blank(&mut self, ctx, drain_timeout)` — implemented for `NetworkFifo`/`UdpTimed`, no-op for `UsbFrameSwap` (frame-swap doesn't queue).

## Public API changes

Approved as breaking — single major version bump.

`ChunkRequest` slims to just what callbacks genuinely need:

```rust
pub struct ChunkRequest {
    pub start: StreamInstant,
    pub pps: u32,
    pub target_points: usize,
}
```

Removed: `min_points`, `buffered_points`, `buffered: Duration`, `device_queued_points`. These leaked the old estimation contract; with idle-policy fallback inside `ChunkProducer`, the callback's job is "produce up to `target_points` — returning fewer triggers idle policy."

`ChunkResult` keeps all three variants (`Filled(n)`, `Starved`, `End`).

`StreamConfig::min_buffer` retires (it only fed `min_points`). `target_buffer` stays (it sizes `target_points`).

`BackendKind::queued_points()` retires from the public surface.

`FifoBackend` trait change: `queued_points()` replaced by `estimator()`. Downstream protocol implementers update once.

## CONTEXT.md additions

Three new load-bearing terms. Drafts:

> **ContentSource**
> The seam where points enter the [OutputModelAdapter](#outputmodel). Two flavours: **FifoContentSource** produces variable-sized chunks for `NetworkFifo` and `UdpTimed` adapters; **FrameContentSource** produces whole frames for `UsbFrameSwap`. [SlicePipeline](#slicepipeline) implements both ([Frame](#frame)-driven). [ChunkProducer](#chunkproducer) implements `FifoContentSource` only (callback-driven).

> **ChunkProducer**
> A [FifoContentSource](#contentsource) that wraps a user-supplied chunk-fill callback and applies idle policy, startup blank, color delay, and `RepeatLast` caching. The streaming-mode analogue of [SlicePipeline](#slicepipeline). Owns the `last_chunk` buffer used for `RepeatLast` underrun handling.

> **BufferEstimator**
> A protocol-owned object that estimates how many [Points](#point) are still queued in a device at a given instant. Each FIFO [Backend](#backend) owns one. Concrete strategies: `SoftwareDecayEstimator` (no telemetry), `StatusDecayEstimator` (periodic authoritative reports), `DualTrackAckEstimator` (UDP sends + ACK correlation, conservative max), `RuntimeAuthorityEstimator` (delegated to an external runtime).

Existing entry for **OutputModel** stays accurate; existing entry for **Backend** gains a sentence about owning a `BufferEstimator` for the FIFO sub-trait.

## Phasing

Each phase compiles, tests, and ships independently. Phases 1–4 are non-breaking on the public crate surface; phase 5 is the single breaking bump.

## Output-behaviour contract

Manual DAC verification is expensive. Phases 1–3 are required to be output-behaviour preserving: same write sizes, same write ordering, same PPS passed to backends, same arm/disarm blanking, same color-delay/startup-blank effects, same retain-on-`WouldBlock`, same reconnect reset, same drain/blank semantics.

Phase 4 is the only phase allowed to make an intentional output-behaviour choice, and only for the known `UdpTimed` stream-vs-frame sizing mismatch documented in [§Phase 4 prerequisite — UdpTimed lock-in](#phase-4-prerequisite--udptimed-lock-in). Any other output drift is a regression.

No phase should require re-testing every physical DAC by hand. Required gates before merging a phase:

- Existing test suite green.
- Fake-backend trace tests for each touched `OutputModel`, asserting write size distribution, PPS, ordering, retain/retry behaviour, arm/disarm blanking, and drain/blank.
- Estimator characterisation tests where old and new estimators are both available, using scripted sends/status/ACKs and asserting equal or deliberately-more-conservative fullness.
- For protocol-specific rewires, transport/runtime fake tests where the protocol already has a fake path.

Physical DAC smoke testing policy:

- Phase 1–3: no broad manual retest unless automated traces diverge.
- Phase 4: smoke one representative `UdpTimed` DAC (LaserCube WiFi-shaped), one `NetworkFifo` DAC, and one `UsbFrameSwap` DAC if available.
- Full all-DAC manual retest only if a gate shows unexplained output drift or the implementation knowingly changes protocol-specific output behaviour.

### Phase 1 — Introduce `BufferEstimator`, migrate FIFO backends (additive only)

This phase is **strictly additive** to the public surface: existing `FifoBackend::queued_points() -> Option<u64>` keeps returning bit-for-bit what it returns today (IDN/LC-USB still `None`, others still their existing telemetry). The new `estimator()` getter is added alongside; nothing observes it yet.

- New `src/buffer_estimate/` module with the read-only `BufferEstimator` trait + four concrete strategies + their tests. Strategies expose protocol-specific event-recording methods directly on the concrete type.
- `FifoBackend` gains `fn estimator(&self) -> &dyn BufferEstimator;`. No trait-level default body — every FIFO backend implements the getter, which is a one-liner returning `&self.estimator` from its concrete field.
- Per-backend changes — each backend holds its strategy as a **concrete named field** and feeds events to it from inside its own protocol code. Backends that today have pps-rebase needs forward `set_point_rate(pps)` to the concrete estimator from inside `try_write_points`:
  - Ether Dream: holds `StatusDecayEstimator`. Status response handler calls `estimator.record_status(now, fullness)`; `try_write_points` calls `estimator.record_send(now, count)` using the **actual** accepted count (`points.len().min(available)`), not the requested length. Delete `decay_fullness`.
  - LaserCube WiFi: replace `lasercube_wifi/dac/buffer_estimator.rs` with `DualTrackAckEstimator` from the shared module (same algorithm, lifted). Backend's send path calls `estimator.record_send(now, message_number, n)`; ACK handler calls `estimator.record_ack(now, message_number, fullness)`. `try_write_points` forwards `set_point_rate` on pps changes (matches today).
  - AVB / oscilloscope: hold a `RuntimeAuthorityEstimator` wrapping the existing `runtime.queued_points()`. No event hooks needed.
  - IDN, LaserCube USB: hold a `SoftwareDecayEstimator`. `try_write_points` calls `estimator.record_send(now, n)` and `estimator.set_point_rate(pps)` on changes.
- `BackendKind::queued_points()` is **unchanged** in this phase. No shim, no semantic drift. The estimator is observable only via the new `estimator()` getter, which nothing reads yet.

End state: all FIFO backends own a concrete estimator and drive its mutation themselves; the public surface is bit-for-bit identical to before. Phase 2 is the first phase where adapter wiring changes.

### Phase 2 — Adapter reads the estimator directly; `src/scheduler.rs` retires

- `NetworkFifoAdapter::step` stops tracking `scheduled_ahead` itself. It calls `backend.estimator().estimated_fullness(now, ctx.pps)` (read-only). The adapter's `scheduled_ahead`, `fractional_consumed`, `last_iteration` fields go away. Send-recording stays inside the backend's `try_write_points` — adapter never touches estimator mutation.
- `UdpTimedAdapter` is metronomic and barely uses estimation today; same treatment for symmetry.
- `BackendKind::queued_points()` removed. Public callers (currently `Stream::run`) keep working because they already use `BackendKind::queued_points()` only via `scheduler::conservative_buffered_points`, which goes away in this phase.
- `src/scheduler.rs` deleted; its tests migrate to `buffer_estimate::software_decay`.

### Phase 3 — Introduce `ContentSource` traits

- New `src/presentation/content_source.rs` with `FifoContentSource` + `FrameContentSource` traits.
- `SlicePipeline` implements both (today's `produce_fifo_chunk` / `produce_frame_swap` become the trait methods, `on_reconnect` consolidates today's `reset_engine` / `reset_color_delay` / `set_pending(last_frame)` / `reset_output_filter(Reconnect)`).
- Adapter step signatures change to `&mut dyn FifoContentSource` / `&mut dyn FrameContentSource`.
- Per-adapter `on_reconnect` keeps its existing role (per-OutputModel scheduler reset). Source-level reconnect happens via `source.on_reconnect(&info)` in the driver, separate from adapter reconnect.

End state: presentation/ uses traits internally; behaviour identical.

### Phase 4 — Build `ChunkProducer`, unify the driver loop

**Prerequisite (blocker):** UdpTimed behaviour lock-in test, see [§Phase 4 prerequisite — UdpTimed lock-in](#phase-4-prerequisite--udptimed-lock-in) below. Must land and stay green before any of phase 4 starts.

- New `ChunkProducer` in `src/stream/chunk_producer.rs`. Owns: the user `F: FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult`, the working buffer (today's `chunk_buffer`), `last_chunk` for `RepeatLast`, color-delay line, startup-blank countdown, idle policy, ended-flag. Implements `FifoContentSource`. `commit_written` is the only place `last_chunk` and `current_instant` are advanced.
- New `src/presentation/driver.rs` containing the unified `run` function. Body lifted from today's `FrameSession::run_loop`. Generalised over `ContentSourceKind` (or two driver functions, one per kind — TBD which is cleaner).
- `try_reconnect` (today in session.rs) and `handle_reconnect` (today in stream/mod.rs) merge into `driver::reconnect`. The compatibility check (frame-swap vs fifo, PPS-in-range) becomes a `ReconnectValidator` closure passed in by each façade.
- `OutputModelAdapter` gains `drain_and_blank(ctx, timeout)`; FIFO/UdpTimed implement it (lifted from `Stream::drain_and_blank`); UsbFrameSwap is a no-op.
- `Stream::run` becomes a 30-line façade: build `ChunkProducer`, pick adapter, call `driver::run`. The 150-line inline scheduler deletes.
- `FrameSession::run_loop` becomes a similar-sized façade: build `SlicePipeline`, pick adapter, call `driver::run`.
- `src/stream/mod.rs` shrinks dramatically. `Stream`, `Dac`, `StreamControl`, `ChunkRequest`, `ChunkResult`, `RunExit`, `StreamStats` stay; everything else moves out.

End state: one driver, one source seam, two façades.

### Phase 5 — Slim `ChunkRequest`, breaking version bump

- Drop `ChunkRequest` fields per "Public API changes" above.
- Drop `StreamConfig::min_buffer`.
- Update examples + tests to the slim surface.
- CHANGELOG entry; major version bump.

### Phase 4 prerequisite — UdpTimed lock-in

Today, `UdpTimed` chunk sizing differs between the two scheduler loops:

- Stream-mode (`src/stream/mod.rs:1270`): `min_points = max_points = max_points_per_chunk` — producer always asked for a full packet.
- FrameSession-mode (`src/presentation/output_model/udp_timed.rs:11`): `CHUNK_SECS = 0.010` — 10ms slices computed from pps.

Phase 4 collapses to one path. Before deciding which behaviour to keep, write a characterisation test against a fake `UdpTimed` backend (LC-WiFi-shaped: small `max_points_per_chunk`, e.g. 140) that runs both modes today and asserts the observed write pattern (chunk size distribution, send cadence, points/sec). Land that test green against the current code as the lock-in baseline.

Then phase 4 makes its sizing choice (preserve full-packet behind a config knob, or converge on 10ms with a documented behaviour change for stream-mode), updates the lock-in test to reflect the new expected pattern, and the diff in the test is the audit trail. Either decision is acceptable; what's not acceptable is "we'll see what happens."

If the test reveals the two behaviours produce indistinguishable output for realistic configs, document that and converge without a config knob. If they diverge, the test makes the divergence explicit and we choose deliberately.

## File-by-file impact (rough)

New files:
- `src/buffer_estimate/mod.rs` (trait + re-exports)
- `src/buffer_estimate/software_decay.rs`
- `src/buffer_estimate/status_decay.rs`
- `src/buffer_estimate/dual_track_ack.rs`
- `src/buffer_estimate/runtime_authority.rs`
- `src/presentation/content_source.rs` (traits)
- `src/presentation/driver.rs` (unified loop)
- `src/stream/chunk_producer.rs` (`ChunkProducer`)

Significantly changed:
- `src/backend.rs` — `FifoBackend::estimator()`, drop `queued_points()`
- `src/presentation/session.rs` — façade only, loop body moved out
- `src/presentation/output_model/{network_fifo,udp_timed,usb_frame_swap}.rs` — take trait objects, drop adapter-side `scheduled_ahead`
- `src/presentation/slice_pipeline.rs` — implement the two traits
- `src/stream/mod.rs` — shrinks substantially; `Stream::run` becomes façade
- All seven protocol `backend.rs` files — hold a `BufferEstimator`

Deleted:
- `src/scheduler.rs`
- `src/protocols/lasercube_wifi/dac/buffer_estimator.rs` (lifted into shared `buffer_estimate/dual_track_ack.rs`)

## Test strategy

The interface is the test surface. Goals: fewer test backdoors, more public-API tests.

- `buffer_estimate/` strategies get focused unit tests per strategy. Existing tests in `src/scheduler.rs` and `lasercube_wifi/dac/buffer_estimator.rs` migrate.
- `ContentSource` traits get conformance tests (a mini test suite each impl runs against itself: `produce → cached_slice → commit_written` round-trips with derived state advancing exactly once; `produce → cached_slice → discard_cached` round-trips with derived state NOT advancing; `on_reconnect` clears all state).
- `OutputModelAdapter` tests already use a fake content source pattern (see `usb_frame_swap.rs::tests`); they migrate to the trait-object signature with a `MockFifoContentSource` / `MockFrameContentSource`.
- `driver::run` gets focused tests with a `MockBackend` + `MockContentSource` covering: control-message arm/disarm/stop, reconnect happy path, reconnect validator rejection, drain-on-end, shutter transitions.
- `Stream::run` integration tests in `src/stream/tests.rs` — keep the public-API ones, drop the ones that asserted on internal `scheduled_ahead` / inline scheduler details (those move to `buffer_estimate` + `driver` tests).
- `presentation/tests.rs` — same treatment. Drop the `#[cfg(test)] pub(crate) use engine::ColorDelayLine;` and `pub(crate) use engine::PresentationEngine;` backdoors once tests stop reaching for them.
- Minimum coverage gate before merging each phase: existing test suite green + new tests for the moved code.

## Risks / open questions

1. **`drain_timeout` shape.** Today only `Stream::drain_and_blank` consults it. After unification, `FrameSession` could optionally expose drain-on-stop too. Out of scope; leave `drain_timeout` stream-only via `StreamConfig` for now.
2. **`SlicePipeline` reset semantics post-trait.** `on_reconnect(&info)` on the trait must consolidate today's separate calls in `FrameSession::try_reconnect` (`reset_engine`, `reset_color_delay`, `set_pending(last_frame)`, `reset_output_filter(Reconnect)`). `SlicePipeline` already owns engine + color-delay + filter, but `last_frame` lives at the session level today. Move `last_frame` into `SlicePipeline` (it has the engine; the engine has the slot) — or keep it at the driver level and pass it back through. Decide in phase 3.
3. **`is_ended()` polling pattern.** Cleaner than `produce_chunk` returning a tri-state, but means the adapter checks `is_ended()` on every step. Acceptable given step is already the hot loop.
4. **Public `BufferEstimator` trait?** External protocol authors registering a custom [Discoverer](#discoverer) may need to implement one. Lean toward `pub` from day one — the `Discoverer` trait is already public, and `FifoBackend` exposes the estimator. Document the four strategies as the canonical menu. The protocol-specific event-recording methods stay on the concrete strategy types and are also public, so downstream protocols can re-use them.
5. **Estimator clock source.** All strategies take `now: Instant` as input. The driver passes `Instant::now()` once per step; backends pass `Instant::now()` from inside `try_write_points` / status handlers / ACK handlers. Tests inject a fake clock by calling methods directly on the concrete strategy. No `Clock` abstraction needed unless test pain emerges.
