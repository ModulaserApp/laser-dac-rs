# Per-Protocol Discovery Plan

## In Short

Move every protocol's discovery code into its own protocol module under `src/protocols/<name>/discovery.rs`, behind a single unified `Discoverer` trait that built-in and external (downstream) protocols both implement. Delete the `DiscoveredDeviceInner` enum. Shrink the crate-root `discovery.rs` from ~1440 LOC to ~300 LOC of registry, identity, and convenience.

This is a *deepening* refactor: the [Discoverer](../CONTEXT.md#discoverer) seam already exists for external DACs but is not used by built-ins. Built-ins currently go through a typed `DiscoveredDeviceInner` variant per protocol, which is structurally a parallel mechanism that solves the same problem with worse [locality](../CONTEXT.md#architectural-terms).

## Problem

`src/discovery.rs` is the largest single file in the crate (1440 LOC) and is the only file that imports from every protocol module simultaneously. Protocol-specific knowledge leaks into it in five places:

1. **`DiscoveredDeviceInner` enum** (`discovery.rs:374-416`) — one variant per protocol carrying that protocol's connect-time payload.
2. **`DacDiscovery` struct fields** (`discovery.rs:814-834`) — one `Option<*Discovery>` per protocol.
3. **`DacDiscovery::scan()`** (`discovery.rs:911-1008`) — feature-gated dispatch to each `*Discovery::scan()`.
4. **`DacDiscovery::connect()`** (`discovery.rs:1071-1122`) — feature-gated match on `device.dac_type`.
5. **`DacDiscovery::dac_type_from_id_prefix()`** (`discovery.rs:1014-1029`) — hardcoded prefix → `DacType` map.

A sixth leak lives in `DiscoveredDeviceInfo::stable_id()` (`discovery.rs:276-347`): a 70-line `match dac_type` switch that knows EtherDream uses MAC, IDN uses hostname, AVB uses slug+index, etc.

Adding a new built-in protocol means editing all six sites. Adding an *external* protocol — already supported via `ExternalDiscoverer` — needs zero edits to `discovery.rs`. Two adapter shapes for the same job is the smell: every built-in protocol is, structurally, an "external" discoverer that happens to live in this crate.

## Decisions Locked In

These were settled during the grilling session before this plan was written. They are not reopened during implementation; if a reason to revisit one appears, raise it as a separate question.

1. **Single `Discoverer` trait.** Built-in and external protocols implement the same trait. The current `ExternalDiscoverer` trait is renamed `Discoverer` and absorbs built-ins.
2. **Opaque connect data.** `Discoverer::connect` takes `Box<dyn Any + Send>`. The `DiscoveredDeviceInner` enum is deleted. Each `Discoverer` impl downcasts inside its own module.
3. **`stable_id` is owned by the `Discoverer`.** It is computed at scan time and stored as a `String` on `DiscoveredDeviceInfo`. The `match dac_type` switch in `stable_id()` is deleted. `DiscoveredDeviceInfo` no longer derives `Hash`/`Eq` over its identifier-hint fields; both are hand-written and consider only `stable_id`. That string *is* the canonical identity per [CONTEXT.md](../CONTEXT.md#stable_id), so two infos compare equal iff their `stable_id`s are equal — IP/MAC/hostname drift between scans no longer changes hash bucket or equality.
4. **`DacCapabilities` snapshotted at scan time.** Each `Discoverer` populates `DiscoveredDevice.caps` when it produces the device. The oscilloscope special case in `DiscoveredDevice::caps()` (`discovery.rs:207-215`) is deleted.
5. **Each protocol owns its discovery code.** Move `HeliosDiscovery` → `protocols/helios/discovery.rs`, etc. The crate-root `discovery.rs` stops `use`-importing from individual protocol modules.
6. **`ExternalDevice` is deleted.** `Discoverer::scan()` returns `DiscoveredDevice` directly.
7. **Per-protocol config moves onto its `Discoverer` impl.** `DacDiscovery::set_idn_scan_addresses` is deleted; callers configure their `IdnDiscoverer` before registering it.
8. **Registration model preserved.** `DacDiscovery::new(EnabledDacTypes)` continues to register all enabled built-ins eagerly. `register()` continues to accept additional discoverers (now typed as `Box<dyn Discoverer>`). The shape is preserved, but every `ExternalDiscoverer` impl downstream must migrate (see *Public API Impact*) — this is a 0.12.0 break, not a no-op.

9. **`Discoverer::prefix()` is unique across registered discoverers.** `DacDiscovery::register` panics on duplicate prefix; the built-ins eagerly registered in `new` are guaranteed unique by construction. `open_by_id` finds the single matching discoverer by prefix and scans only it. This makes the lookup deterministic and forecloses the "two `IdnDiscoverer`s registered" ambiguity.

10. **IDN custom-address discovery uses `register()` after skipping the built-in.** `DacDiscovery::new` accepts an `EnabledDacTypes` that can mask out IDN; integration tests that need custom scan addresses construct `DacDiscovery::new(EnabledDacTypes::default().without(DacType::Idn))` and then `register(Box::new(IdnDiscoverer::with_scan_addresses(addrs)))`. No `replace_discoverer` API is added — the mask + register pattern is sufficient and keeps `DacDiscovery` minimal.

## Target Architecture

### The `Discoverer` trait

```rust
// src/discovery.rs
pub trait Discoverer: Send {
    /// The DAC type this discoverer handles.
    fn dac_type(&self) -> DacType;

    /// Stable id prefix this discoverer's devices use, e.g. `"etherdream"`.
    /// Used by `open_by_id` to narrow scans to a single discoverer when the
    /// caller's id has a known prefix.
    fn prefix(&self) -> &str;

    /// Locate reachable DACs of `dac_type()` on the system / network.
    /// Each returned `DiscoveredDevice` must have its `stable_id` and `caps`
    /// populated.
    fn scan(&mut self) -> Vec<DiscoveredDevice>;

    /// Open a connection to a previously-scanned device. `opaque` is the
    /// `connect_data` field from a `DiscoveredDevice` this same discoverer
    /// produced.
    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind>;
}
```

### `DiscoveredDevice` and `DiscoveredDeviceInfo`

```rust
pub struct DiscoveredDevice {
    info: DiscoveredDeviceInfo,
    caps: DacCapabilities,
    /// Index into the `DacDiscovery` registry: which `Discoverer` produced this.
    discoverer_index: usize,
    /// Opaque connect-time data, downcast inside the producing discoverer.
    connect_data: Box<dyn Any + Send>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiscoveredDeviceInfo {
    pub dac_type: DacType,
    pub stable_id: String,        // canonical, set by Discoverer at scan time
    pub name: String,             // human-readable, set by Discoverer at scan time
    pub ip_address: Option<IpAddr>,
    pub mac_address: Option<[u8; 6]>,
    pub hostname: Option<String>,
    pub usb_address: Option<String>,
    pub hardware_name: Option<String>,
    pub device_index: Option<u16>,
}
```

`DiscoveredDeviceInfo` keeps its current "flat union of identifier hints" shape — these are useful to UIs and CLIs for filtering/display — but `stable_id` is no longer derived from them. It is authored by the discoverer.

### `DacDiscovery` registry

```rust
pub struct DacDiscovery {
    discoverers: Vec<Box<dyn Discoverer>>,
    enabled: EnabledDacTypes,
}

impl DacDiscovery {
    pub fn new(enabled: EnabledDacTypes) -> Self {
        let mut this = Self { discoverers: Vec::new(), enabled };
        // Eagerly register all built-ins available under current features.
        // Failable ones (Helios USB, LaserCube USB) are silently skipped if
        // their controller fails to initialize, matching today's behavior.
        #[cfg(feature = "helios")]
        if let Some(d) = HeliosDiscoverer::new() { this.register_box(Box::new(d)); }
        #[cfg(feature = "ether-dream")]
        this.register_box(Box::new(EtherDreamDiscoverer::new()));
        // ... etc for each feature
        this
    }

    pub fn register(&mut self, d: Box<dyn Discoverer>) { self.discoverers.push(d); }

    pub fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let mut out = Vec::new();
        for (idx, d) in self.discoverers.iter_mut().enumerate() {
            if !self.enabled.is_enabled(d.dac_type()) { continue; }
            for mut device in d.scan() {
                device.discoverer_index = idx;
                out.push(device);
            }
        }
        out
    }

    pub fn connect(&mut self, device: DiscoveredDevice) -> Result<BackendKind> {
        let DiscoveredDevice { discoverer_index, connect_data, .. } = device;
        self.discoverers
            .get_mut(discoverer_index)
            .ok_or_else(|| Error::invalid_config("discoverer not found"))?
            .connect(connect_data)
    }
}
```

`open_by_id`'s prefix-narrowing optimization becomes: walk discoverers, find the one whose `prefix()` matches the id's prefix, scan only that one.

### Module layout

```
src/discovery.rs                          ~300 LOC
    ├ Discoverer trait
    ├ DiscoveredDevice / DiscoveredDeviceInfo
    ├ DacDiscovery registry + open_by_id
    ├ slugify_device_id helper (used by several protocols)
    └ unit tests for registry behavior

src/protocols/helios/discovery.rs         ~80 LOC   (HeliosDiscoverer + tests)
src/protocols/ether_dream/discovery.rs    ~90 LOC   (EtherDreamDiscoverer + tests)
src/protocols/idn/discovery.rs            ~100 LOC  (IdnDiscoverer + tests, incl. testutils)
src/protocols/lasercube_wifi/discovery.rs ~90 LOC
src/protocols/lasercube_usb/discovery.rs  ~70 LOC
src/protocols/avb/discovery.rs            ~70 LOC
src/protocols/oscilloscope/discovery.rs   ~70 LOC   (carries sample-rate-aware caps)
```

Each protocol's `discovery.rs` owns: that protocol's `Discoverer` impl, its `stable_id` formatting, its connect-data downcast, its `caps` snapshotting, and its identity-formatting unit tests.

## Public API Impact

The crate is on `0.11.7`, pre-1.0, so a breaking change is acceptable. This is a real `0.12.0` break — not silent. Concretely:

**Removed:**
- `pub trait ExternalDiscoverer` — replaced by `Discoverer` (same shape, slightly extended).
- `pub struct ExternalDevice` — `Discoverer::scan` returns `DiscoveredDevice` directly.
- `DacDiscovery::set_idn_scan_addresses` — moves to `IdnDiscoverer::with_scan_addresses` (testutils only).

**Renamed / extended:**
- `ExternalDiscoverer::{dac_type, scan, connect}` → `Discoverer::{dac_type, prefix, scan, connect}` (gains `prefix()`; `scan` returns `Vec<DiscoveredDevice>`).

**Mostly unchanged:**
- `open_device(id)`, `open_device_with(...)`, `list_devices()` — work the same.
- `DacDiscovery::new(EnabledDacTypes)` — works the same.
- `DacDiscovery::scan() / connect()` — work the same.
- `DacDiscovery::register()` — same name, but takes `Box<dyn Discoverer>` instead of `Box<dyn ExternalDiscoverer>` (and may panic on duplicate prefix per Decision #9).
- `DiscoveredDeviceInfo` field names — unchanged for existing fields, but `stable_id: String` is added as a new public field. `info.stable_id()` is preserved as an accessor method to avoid churn at call sites; the `Hash`/`Eq` change (Decision #3) is the silent one to call out in the CHANGELOG.

**Callers that must migrate (every impl must rename + add `prefix()` + return `Vec<DiscoveredDevice>`):**

| Impl site                          | Notes                                              |
|------------------------------------|----------------------------------------------------|
| `src/stream/tests.rs:3031`         | `MockExternalDiscoverer` — rename to `MockDiscoverer` and rewrite scan return |
| `src/presentation/tests.rs:1701`   | Test-local discoverer impl                         |
| `src/presentation/tests.rs:1725`   | Test-local discoverer impl                         |
| Any downstream crate impl          | Same migration; sketch shown in CHANGELOG snippet  |

**CHANGELOG snippet for `0.12.0`:**

```text
BREAKING: `ExternalDiscoverer` has been renamed to `Discoverer` and absorbed
the built-in protocols.

Migration for downstream impls:
  - Rename `impl ExternalDiscoverer for X` → `impl Discoverer for X`.
  - Add `fn prefix(&self) -> &str` returning your protocol slug
    (e.g., "myproto").
  - Change `fn scan(&mut self) -> Vec<ExternalDevice>` to
    `fn scan(&mut self) -> Vec<DiscoveredDevice>`. Replace `ExternalDevice`
    construction with `DiscoveredDevice`; populate `stable_id` and `caps`
    yourself (was previously auto-derived from your dac_type).

Behavioral change:
  - `DiscoveredDeviceInfo` now hashes/compares by `stable_id` only. If you
    used `DiscoveredDeviceInfo` as a `HashMap` key relying on structural
    (mac/ip/hostname) equality, switch to keying on `stable_id` directly.

  - `DacDiscovery::register` panics if two discoverers share the same prefix.

Accessor signature changes:
  - `DiscoveredDevice::name() -> &str` (was `String`).
  - `DiscoveredDevice::info() -> &DiscoveredDeviceInfo` (was owned).
    If you need an owned copy, use `device.info().clone()`.
```

## Transitional Shapes

The "target shape" of `DiscoveredDevice` and `DiscoveredDeviceInfo` shown above only materializes after step 9b. Steps 2-9a operate on transitional shapes that allow old and new code paths to coexist without flag days.

**During steps 2-9a, `DiscoveredDevice` carries:**

```rust
pub struct DiscoveredDevice {
    info: DiscoveredDeviceInfo,
    caps: Option<DacCapabilities>,           // None = derive via legacy path
    inner: DiscoveredDeviceInner,            // legacy enum, kept until step 9b
    discoverer_index: Option<usize>,         // Some = produced by new-style Discoverer
    connect_data: Option<Box<dyn Any + Send>>, // Some = use new-style connect path
}
```

**During steps 2-8, `DiscoveredDeviceInfo` carries:**

```rust
pub struct DiscoveredDeviceInfo {
    pub dac_type: DacType,
    pub stable_id: Option<String>,           // None = fall back to derived stable_id()
    // ...existing fields unchanged
}
```

**Tightening rules:**

- Step 2 lands the new fields as `Option<>`; the legacy derivation paths populate them (so callers that read them never see `None` after a fresh `scan()`).
- Each of steps 3-8 migrates one protocol to populate `discoverer_index`/`connect_data` and to author its own `stable_id`/`caps` instead of going through `DiscoveredDeviceInner::*` and the `match dac_type` switch.
- Step 9a deletes the legacy switch and the per-protocol fields on `DacDiscovery`. The `Option<>` wrappers are still present.
- Step 9b deletes `DiscoveredDeviceInner` and the `inner` field, then tightens every `Option<>` to its non-optional target shape in one commit. After this commit the struct matches the Target Architecture exactly.

**Why both fields coexist:** `DacDiscovery::connect` currently consumes `device.inner` by move (`discovery.rs:1073, 1087`). To run new and old paths in parallel, the connect dispatcher checks `connect_data` first and falls back to matching `inner`. Both fields must be owned by the struct; you can't have one moved out and the other still readable. This is the cost of "every step compiles."

## Migration Plan

Ten landable steps (counting 9a/9b separately). Each step compiles and passes tests on its own — provided the [Transitional Shapes](#transitional-shapes) are followed. Steps 1-2 establish the new seam; 3-8 move protocols one at a time; 9a deletes internal-only dead code; 10 renames the trait and deletes `ExternalDevice` (the only public-API break); 9b collapses the dual registry and tightens the transitional `Option<>` fields to their target shapes.

### Step 1 — Introduce the `Discoverer` trait alongside the existing code

In `discovery.rs`, add the new `Discoverer` trait (final shape) and a temporary blanket impl that lets the registry treat existing `ExternalDiscoverer`s uniformly. Don't touch built-ins yet. Compile.

### Step 2 — Add transitional `Option<>` fields on `DiscoveredDevice` / `DiscoveredDeviceInfo`

Add `caps: Option<DacCapabilities>`, `discoverer_index: Option<usize>`, `connect_data: Option<Box<dyn Any + Send>>` to `DiscoveredDevice`, and `stable_id: Option<String>` to `DiscoveredDeviceInfo`. Keep `inner: DiscoveredDeviceInner` untouched. See [Transitional Shapes](#transitional-shapes).

Populate the new fields from the existing per-protocol logic inside `DacDiscovery::scan()` so every device returned from a scan has them set. `DiscoveredDeviceInfo::stable_id()` returns the stored `Option`'s value if present, otherwise falls back to the existing derived path — the giant `match dac_type` switch stays alive but becomes a fallback. It's deleted in step 9a.

Because the new fields are `Option<>`, every existing struct literal continues to compile (`..Default::default()` or explicit `None` is added at each construction site: `discovery.rs:967-976`, `stream/tests.rs:3038`, `presentation/tests.rs:1707, 1736`). Adjust the `Hash`/`Eq` derive on `DiscoveredDeviceInfo` per Decision #3: hand-write both to consider only `stable_id` when it is `Some`, and fall back to the structural derivation when it is `None`. After step 9b tightens `stable_id` to a non-optional `String`, the fallback branch goes away.

Compile, run tests with `--all-features` AND with each protocol feature in isolation (see *Risk and Reversibility*).

### Step 3 — Move `HeliosDiscovery` → `protocols/helios/discovery.rs` as `HeliosDiscoverer: Discoverer`

- Define `HeliosConnectData` (private) holding the `HeliosDac` formerly in `DiscoveredDeviceInner::Helios`.
- `scan()` produces `DiscoveredDevice { connect_data: Box::new(HeliosConnectData { dac }), stable_id: format!("helios:{name}"), caps: caps_for_dac_type(&DacType::Helios), .. }`.
- `connect()` downcasts to `HeliosConnectData`.
- Register via the new path in `DacDiscovery::new` AND keep the old `helios: Option<HeliosDiscovery>` field producing the old `DiscoveredDeviceInner::Helios` variant in parallel, behind a feature flag or compile-time switch — actually, simpler: gate the *consumer* side. `DacDiscovery::scan` produces both old-style and new-style results, but in this step only Helios uses new-style. `DacDiscovery::connect` checks `connect_data` first; if absent, falls back to the old `inner` enum. This keeps every other protocol working.
- Move Helios's identity tests into `protocols/helios/discovery.rs`.

Compile, run tests, run any Helios hardware example you have.

### Steps 4-8 — Repeat step 3 for each remaining protocol, one per step

Order suggestion (smallest blast radius first):
- Step 4: AVB
- Step 5: LaserCube USB
- Step 6: Oscilloscope (this is also where the sample-rate-dependent caps move into the discoverer; the special case in `DiscoveredDevice::caps()` is removed in this step)
- Step 7: LaserCube WiFi
- Step 8: Ether Dream and IDN together, since they share the network broadcast feel and IDN carries the testutils API. `DacDiscovery::set_idn_scan_addresses` is deleted in this step. Per Decision #10, `tests/idn_e2e.rs` migrates to: build `DacDiscovery::new(EnabledDacTypes::default().without(DacType::Idn))`, then `register(Box::new(IdnDiscoverer::with_scan_addresses(addrs)))`. Verify `EnabledDacTypes::without` exists or add it as part of this step.

### Step 9a — Delete internal dead pieces (no public API change)

- The per-protocol fields on `DacDiscovery` (`helios: Option<...>`, `etherdream: ...`, etc.)
- The 70-line `match dac_type` switch in `DiscoveredDeviceInfo::stable_id()` (the stored `Option<String>` is now always `Some` after a fresh scan)
- `dac_type_from_id_prefix` (replaced by `Discoverer::prefix()` lookup with uniqueness guarantee per Decision #9)

The `external: Vec<Box<dyn ExternalDiscoverer>>` field, the `DiscoveredDeviceInner::External` arm, the `inner` field on `DiscoveredDevice`, and the rest of `DiscoveredDeviceInner` are still alive — they go in 9b after the rename.

### Step 10 — Rename `ExternalDiscoverer` → `Discoverer`, delete `ExternalDevice`

This must precede 9b: `external: Vec<Box<dyn ExternalDiscoverer>>` and `DiscoveredDeviceInner::External` cannot be deleted while the trait is alive under its old name.

- `ExternalDiscoverer` is renamed to `Discoverer`. Update its single re-export.
- `ExternalDevice` is deleted; its three remaining call sites (`stream/tests.rs:3031`, `presentation/tests.rs:1701`, `presentation/tests.rs:1725`) migrate to `DiscoveredDevice` constructors. Each impl gains `prefix()`.
- Update `lib.rs` re-exports and any doc examples.
- Add the `0.12.0` CHANGELOG entry from *Public API Impact* covering rename + `prefix()` + `Vec<DiscoveredDevice>` return + `Hash`/`Eq` change + duplicate-prefix panic.

### Step 9b — Collapse the dual registry and tighten transitional shapes

Now that there is only one trait, fold `external: Vec<Box<dyn Discoverer>>` into the single `discoverers: Vec<Box<dyn Discoverer>>`. Then:

- Delete the `DiscoveredDeviceInner` enum.
- Delete the `inner: DiscoveredDeviceInner` field on `DiscoveredDevice`.
- Delete the fallback path in `DacDiscovery::connect`.
- Tighten every transitional `Option<>` to its target shape in one commit: `caps: DacCapabilities`, `discoverer_index: usize`, `connect_data: Box<dyn Any + Send>`, `stable_id: String`. The `Hash`/`Eq` impls drop their `Option<String>` branches and become straight `stable_id` comparisons.

After this step `DiscoveredDevice` matches the Target Architecture exactly.

## Test Surface

The point of this refactor is to make discovery more testable. After the refactor:

- **Per-protocol identity tests** live next to the protocol. EtherDream's MAC formatting, IDN's hostname rules, AVB's slug+index formatting are unit-tested inside their own discovery files. The shared identity tests in `discovery.rs::tests` move out, except the registry-level ones (multiple discoverers, `EnabledDacTypes` filtering, `open_by_id` prefix narrowing).
- **Mock discoverer for registry tests** is unchanged — the existing `MockExternalDiscoverer` already implements the trait we're consolidating to. It stays in `discovery.rs::tests` as the canonical example for downstream implementers.
- **Per-protocol scan tests** (e.g. against IDN's mock server) become easier: a test can construct `IdnDiscoverer::new().with_scan_addresses(addrs)` and call `scan()` directly without standing up a full `DacDiscovery`.

No existing test should need rewriting beyond the field-vs-method change for `stable_id` (step 2).

## Risk and Reversibility

- **Reversibility:** every step compiles independently. Step 2 adds transitional fields as `Option<>` (not pure additions in the strict sense — every struct literal is touched, but no field becomes required). Steps 3-8 each touch one protocol module and one fallback branch in `DacDiscovery`. Step 9a is internal-only deletion; step 10 is the rename + `ExternalDevice` removal; step 9b is the deletion-heavy finalizer. If any step uncovers a reason to abandon the refactor, revert that step and earlier.
- **Hardware risk:** discovery code paths are exercised by every example and every CI test. Run `examples/stream.rs` against any available real DAC after step 3 (Helios is most accessible) and step 8 (network DACs).
- **External-discoverer downstream users:** the rename in step 10 is the only public-API break. Pre-step-10 the trait still works under its old name. If there's a known downstream user, defer step 10 (and therefore 9b) until they're ready; steps 1-9a can land independently.
- **Feature-matrix coverage:** every protocol is feature-gated, so default-feature builds do not exercise every code path being refactored. After each step, run:
  - `cargo test --no-default-features` (registry + identity only)
  - `cargo test --all-features` (everything together)
  - For at least one step in 3-8, run `cargo test --no-default-features --features <one-protocol>` per protocol to catch feature-gated breakage. This catches cases where a `#[cfg(feature = "x")]` block referenced a transitional field that only existed under another feature.

## Out of Scope

- Parallelizing `scan()` across discoverers. Today it's sequential; this refactor keeps it sequential.
- Async discovery. Today `scan()` is blocking; this refactor keeps it blocking.
- Re-shaping `DiscoveredDeviceInfo` into a per-protocol typed alternative (Option B from the design conversation). The flat-fields shape is preserved for UI/CLI ergonomics.
