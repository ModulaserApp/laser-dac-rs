# Rust API Guidelines Review for `laser-dac` Crate

This review evaluates the `laser-dac` crate (v0.3.1) against the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/).

## Summary

| Category | Status | Notes |
|----------|--------|-------|
| Naming | Good | Minor issues |
| Documentation | Good | Minor improvements possible |
| Interoperability | Needs Work | Missing some common traits |
| Type Safety | Good | |
| Dependability | Good | |
| Debuggability | Good | |
| Future Proofing | Needs Attention | Public fields, unsealed traits |
| Necessities | Good | |

---

## Naming (C-CASE, C-CONV, C-GETTER, C-ITER, C-FEATURE, C-WORD-ORDER)

### C-CASE: Casing conforms to RFC 430

**Status: PASS**

- Types use `UpperCamelCase`: `LaserPoint`, `LaserFrame`, `DacType`, `DacBackend`
- Functions use `snake_case`: `write_frame`, `is_connected`, `dac_type`
- Constants/statics use `SCREAMING_SNAKE_CASE` where applicable
- Acronyms are handled correctly: `Dac` (not `DAC`), `Usb` (not `USB`)

### C-CONV: Ad-hoc conversions follow `as_`, `to_`, `into_` conventions

**Status: PASS**

- `from_dac()` correctly uses `from_` prefix for constructors
- `from_discovery()` follows convention
- No incorrect `as_`/`to_`/`into_` usage found

### C-GETTER: Getter names follow Rust convention

**Status: PASS**

- Getters don't use `get_` prefix: `device_name()`, `dac_type()`, `state()`
- Boolean getters use appropriate names: `is_connected()`, `is_enabled()`, `is_alive()`, `is_empty()`

### C-ITER: Methods on collections that produce iterators follow conventions

**Status: PASS**

- `EnabledDacTypes::iter()` returns `impl Iterator` (correct)

### C-FEATURE: Feature names are free of placeholder words

**Status: PASS**

- Features use direct names: `helios`, `ether-dream`, `idn`, `lasercube-wifi`, `lasercube-usb`
- No `use-` or `with-` prefixes
- `serde` feature correctly named (not `serde_impls`)

### C-WORD-ORDER: Names use consistent word order

**Status: PASS with minor notes**

- Consistent pattern: `{Protocol}Backend`, `{Protocol}Discovery`
- Type names are clear: `LaserPoint`, `LaserFrame`

---

## Documentation (C-CRATE-DOC, C-EXAMPLE, C-QUESTION-MARK, C-FAILURE, C-LINK)

### C-CRATE-DOC: Crate level docs are thorough and include examples

**Status: PASS**

`src/lib.rs:1-28` contains:
- Short summary of purpose
- List of supported DACs
- Feature flag documentation
- Coordinate system explanation

### C-EXAMPLE: Examples use `?`, not `try!`, not `unwrap`

**Status: MIXED**

- Doc examples in `src/worker.rs:40-57` are marked `ignore` (acceptable for hardware-dependent code)
- `examples/unified.rs` demonstrates proper API usage but doesn't show error handling patterns
- Consider adding `# Errors` sections to fallible functions

### C-QUESTION-MARK: Function docs include error conditions

**Status: NEEDS IMPROVEMENT**

Many fallible functions lack `# Errors` documentation:
- `DacBackend::connect()` - no error docs
- `DacBackend::write_frame()` - no error docs
- `UnifiedDiscovery::connect()` - no error docs

**Recommendation**: Add `# Errors` sections documenting when functions return `Err`.

### C-FAILURE: Functions document panic conditions

**Status: PASS**

- No panicking functions found in public API
- Functions return `Result` for fallible operations

### C-LINK: Prose contains hyperlinks to relevant types

**Status: COULD IMPROVE**

- Some doc comments reference types without markdown links
- Consider adding intra-doc links like `[`LaserFrame`]`

---

## Interoperability (C-COMMON-TRAITS, C-CONV-TRAITS, C-SEND-SYNC, C-GOOD-ERR, C-SERDE)

### C-COMMON-TRAITS: Types eagerly implement common traits

**Status: NEEDS IMPROVEMENT**

| Type | Debug | Clone | Copy | Eq | PartialEq | Hash | Default | Display |
|------|-------|-------|------|-----|-----------|------|---------|---------|
| `LaserPoint` | Yes | Yes | Yes | No | Yes | No | No | No |
| `LaserFrame` | Yes | Yes | No | No | Yes | No | No | No |
| `DacType` | Yes | Yes | Yes | Yes | Yes | Yes | No | Yes |
| `WriteResult` | Yes | Yes | Yes | Yes | Yes | No | No | No |
| `DacDevice` | Yes | Yes | No | No | No | No | No | No |
| `DiscoveredDac` | Yes | Yes | No | No | No | No | No | No |
| `Error` | Yes | No | No | No | No | No | No | Yes* |

**Issues**:
1. `LaserPoint` should implement `Default` (all zeros is sensible)
2. `LaserFrame` should implement `Default` (empty frame is sensible)
3. `DacDevice` and `DiscoveredDac` are missing `PartialEq`, `Eq`
4. `Error` is not `Clone` - this is common but worth noting

### C-CONV-TRAITS: Conversions use standard traits From, AsRef, AsMut

**Status: PASS**

- Good use of `From<&LaserPoint>` for protocol-specific point types
- `From` implementations found in protocol modules for type conversions

### C-SEND-SYNC: Types are Send and Sync where possible

**Status: NEEDS VERIFICATION**

- `Error` uses `Box<dyn StdError + Send + Sync>` - correct
- `DacBackend` trait requires `Send + 'static` - correct
- No explicit tests for `Send`/`Sync` bounds found

**Recommendation**: Add compile-time assertions:
```rust
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    fn assert_all() {
        assert_send::<Error>();
        assert_send::<LaserPoint>();
        assert_sync::<LaserPoint>();
    }
};
```

### C-GOOD-ERR: Error types are meaningful and well-behaved

**Status: PASS**

- `Error` implements `std::error::Error` via `thiserror`
- Has `#[source]` for wrapped errors
- Has `Display` implementation
- Error is `Send + Sync` capable (uses `Box<dyn StdError + Send + Sync>`)

### C-SERDE: Data structures implement Serde's Serialize, Deserialize

**Status: PASS**

- `DacType` has conditional serde: `#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]`
- `EnabledDacTypes` has conditional serde
- Feature is correctly named `serde`

---

## Type Safety (C-NEWTYPE, C-CUSTOM-TYPE, C-BITFLAG)

### C-NEWTYPE: Newtypes provide static distinctions

**Status: PASS**

- `LaserPoint` wraps raw coordinates with semantic meaning
- `LaserFrame` encapsulates points with PPS
- Protocol-specific types prevent mixing (e.g., `HeliosPoint` vs `DacPoint`)

### C-CUSTOM-TYPE: Arguments convey meaning through types

**Status: PASS**

- Uses `DacType` enum instead of strings
- Uses `EnabledDacTypes` instead of raw `HashSet`
- `WriteResult` enum instead of boolean

### C-BITFLAG: Bitflag types use bitflags crate

**Status: PASS**

- `WriteFrameFlags` in helios module uses `bitflags` crate (Cargo.toml:36)

---

## Dependability (C-VALIDATE, C-DTOR-FAIL, C-DTOR-BLOCK)

### C-VALIDATE: Functions validate their arguments

**Status: ACCEPTABLE**

- Coordinates are documented as normalized (-1.0 to 1.0) but not validated
- This is acceptable for performance-critical laser output
- Consider documenting behavior for out-of-range values

### C-DTOR-FAIL: Destructors never fail

**Status: PASS**

- `DacWorker::drop()` uses `try_send` which cannot block or panic
- Backend disconnect operations in Drop use `let _ =` to ignore errors

### C-DTOR-BLOCK: Destructors that may block have alternatives

**Status: PASS**

- `DacWorker` sends stop command via channel (non-blocking)
- No blocking cleanup operations found

---

## Debuggability (C-DEBUG, C-DEBUG-NONEMPTY)

### C-DEBUG: All public types implement Debug

**Status: PASS**

All public types have `#[derive(Debug)]`:
- `LaserPoint`, `LaserFrame`, `DacType`, `WriteResult`
- `DacDevice`, `DiscoveredDac`, `DacConnectionState`
- `Error`, `WorkerCommand`, `WorkerStatus`
- `EnabledDacTypes`, etc.

### C-DEBUG-NONEMPTY: Debug representation is never empty

**Status: PASS**

- All Debug derives include meaningful field information
- Enum variants show their names and contents

---

## Future Proofing (C-SEALED, C-STRUCT-PRIVATE, C-STRUCT-BOUNDS, C-NEWTYPE-HIDE)

### C-SEALED: Traits that are not meant to be implemented are sealed

**Status: NEEDS WORK**

`DacBackend` trait appears to be internal-implementation focused but is not sealed:
```rust
pub trait DacBackend: Send + 'static {
    fn dac_type(&self) -> DacType;
    fn connect(&mut self) -> Result<()>;
    // ...
}
```

**Recommendation**: If users shouldn't implement this trait, seal it:
```rust
mod private {
    pub trait Sealed {}
}
pub trait DacBackend: private::Sealed + Send + 'static { ... }
```

### C-STRUCT-PRIVATE: Structs have private fields

**Status: NEEDS WORK**

Several public structs have all-public fields:

1. `LaserPoint` (src/types.rs:24-37):
```rust
pub struct LaserPoint {
    pub x: f32,
    pub y: f32,
    pub r: u16,
    // ...
}
```

2. `LaserFrame` (src/types.rs:67-72):
```rust
pub struct LaserFrame {
    pub pps: u32,
    pub points: Vec<LaserPoint>,
}
```

3. `DacDevice` (src/types.rs:197-200):
```rust
pub struct DacDevice {
    pub name: String,
    pub dac_type: DacType,
}
```

4. `DiscoveredDac` (src/types.rs:219-230) - all fields public

**Implications**:
- Cannot add fields without breaking changes
- Cannot enforce invariants

**Recommendation**: Make fields private and provide getters, or add `#[non_exhaustive]` attribute.

### C-STRUCT-BOUNDS: Data structures do not duplicate trait bounds

**Status: PASS**

No trait bounds on struct definitions found - bounds are only on impl blocks.

### C-NEWTYPE-HIDE: Newtypes encapsulate implementation details

**Status: PARTIAL**

- `EnabledDacTypes` correctly hides `HashSet<DacType>` implementation
- `LaserPoint` and `LaserFrame` expose all fields directly

---

## Necessities (C-STABLE, C-PERMISSIVE)

### C-STABLE: Public dependencies of stable crate are stable

**Status: PASS**

All dependencies are stable releases:
- `thiserror = "1.0"`
- `bitflags = "2"`
- `byteorder = "1.5"`
- `rusb = "0.9"` (optional)
- `socket2 = "0.5"` (optional)
- `serde = "1.0"` (optional)

### C-PERMISSIVE: Crate and its dependencies have permissive license

**Status: PASS**

- Crate is MIT licensed (`license = "MIT"` in Cargo.toml)
- All dependencies use permissive licenses (MIT, Apache-2.0, or similar)

---

## Additional Observations

### Crate Name

**Status: PASS**

- Name is `laser-dac`, not `laser-dac-rs` - correctly avoids `-rs` suffix per guidelines

### Feature Flag Organization

**Status: EXCELLENT**

Good hierarchical organization:
```toml
default = ["all-dacs"]
all-dacs = ["usb-dacs", "network-dacs"]
usb-dacs = ["helios", "lasercube-usb"]
network-dacs = ["ether-dream", "idn", "lasercube-wifi"]
```

### docs.rs Configuration

**Status: PASS**

```toml
[package.metadata.docs.rs]
features = ["all-dacs"]
```

---

## Priority Recommendations

### High Priority

1. **Add `#[non_exhaustive]` to public structs** to allow adding fields in the future:
   ```rust
   #[non_exhaustive]
   pub struct LaserPoint { ... }
   ```

2. **Document error conditions** with `# Errors` sections on fallible functions

3. **Add missing trait implementations**:
   - `Default` for `LaserPoint` and `LaserFrame`
   - `PartialEq`/`Eq` for `DacDevice` and `DiscoveredDac`

### Medium Priority

4. **Consider sealing `DacBackend`** if external implementations aren't intended

5. **Add Send/Sync compile-time tests** to prevent accidental regressions

6. **Add intra-doc links** for better documentation navigation

### Low Priority

7. **Consider making struct fields private** with accessor methods for stronger encapsulation

---

## Compliance Summary

| Guideline | Status |
|-----------|--------|
| C-CASE | PASS |
| C-CONV | PASS |
| C-GETTER | PASS |
| C-ITER | PASS |
| C-FEATURE | PASS |
| C-WORD-ORDER | PASS |
| C-CRATE-DOC | PASS |
| C-EXAMPLE | PASS |
| C-QUESTION-MARK | NEEDS WORK |
| C-FAILURE | PASS |
| C-LINK | COULD IMPROVE |
| C-COMMON-TRAITS | NEEDS WORK |
| C-CONV-TRAITS | PASS |
| C-SEND-SYNC | NEEDS VERIFICATION |
| C-GOOD-ERR | PASS |
| C-SERDE | PASS |
| C-NEWTYPE | PASS |
| C-CUSTOM-TYPE | PASS |
| C-BITFLAG | PASS |
| C-VALIDATE | ACCEPTABLE |
| C-DTOR-FAIL | PASS |
| C-DTOR-BLOCK | PASS |
| C-DEBUG | PASS |
| C-DEBUG-NONEMPTY | PASS |
| C-SEALED | NEEDS WORK |
| C-STRUCT-PRIVATE | NEEDS WORK |
| C-STRUCT-BOUNDS | PASS |
| C-NEWTYPE-HIDE | PARTIAL |
| C-STABLE | PASS |
| C-PERMISSIVE | PASS |

**Overall**: The crate follows most Rust API guidelines well. The main areas for improvement are:
1. Future-proofing with `#[non_exhaustive]` attributes
2. Adding missing common trait implementations
3. Documenting error conditions

---

*Review based on [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/) - December 2025*
