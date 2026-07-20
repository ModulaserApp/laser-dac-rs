# Helios USB DAC — ground-truth capture fixtures

These fixtures are **real traces captured from a physical Helios Laser DAC**
(USB `1209:E500`, firmware `5`, control-name `Helios 909127731`), used to ground
the hermetic replay tests for the Helios USB path so the scripted bytes and
frame-swap cadence are confirmed against silicon rather than guessed.

## Provenance

Captured on a Raspberry Pi with the DAC attached over USB, using a throwaway
standalone `rusb 0.9` program that mimics the production wire protocol
(`src/protocols/helios/native.rs`) exactly and records every USB op with
per-operation timing to JSONL. **No laser was connected** — all frames are
blanked (zero-color, zero-intensity) points. The capture tool is scaffolding
and is intentionally *not* committed.

Each JSONL event: `seq`, `phase`, `op`, `ep`, `req` (hex), `resp` (hex),
`status` (`ok`/`timeout`/`pipe`/`io`/…), `n` (bytes transferred, `-1` on error),
`dt_us` (gap since previous op), `dur_us` (op duration), `t_us` (absolute since
capture start), `note`.

## Files

| File | Scenario |
|---|---|
| `init.jsonl` | Full open handshake: claim, set_alt (~10ms), 100ms settle, drain (0 pkts), firmware probe, send SDK version. |
| `identity.jsonl` | Firmware ×5 + name ×5 (stability) + USB string descriptors. |
| `status_idle.sample.jsonl` | Idle `GET_STATUS` poll (trimmed to 20 of 200 events; all `83 01` Ready). |
| `frame_sizes.jsonl` | Bulk writes for sizes {1,44,45,100,109,1000,4095,5000-oversize}. |
| `pps_sweep.jsonl` | pps {1,100,1000,30000,65535,100000-over-max}; also captures back-pressure. |
| `stopshutter.jsonl` | `STOP` / `SET_SHUTTER` OUT-only writes. |
| `frame_cadence.summary.json` | Distilled per-pps NotReady durations (from 6935-event raw capture; cycle-0 outliers excluded). |

## Ground-truth headlines

**Real response bytes (variable-length interrupt packets — NOT padded to 32):**
- Status: `83 01` (Ready) / `83 00` (NotReady) — **2 bytes**
- Firmware: `84 05 00 00 00` → fw = 5 — **5 bytes**
- Name: `85 48656c696f7320393039313237373331 00 <junk…>` → `"Helios 909127731"` — **full 32 bytes with uninitialized garbage after the NUL**

**Init timing:** `claim` ~25µs, `set_alternate_setting` **~10ms**, 100ms settle,
drain finds **0 packets** (first IN read times out), firmware probe succeeds on
attempt 1. Every interrupt transfer costs **~1ms** (USB full-speed 1ms
bInterval) → each control round-trip ≈ **2ms**.

**Frame-swap cadence** (500-pt blanked frame, NotReady window = write→first Ready):

| pps | ideal playtime | measured NotReady (median) |
|---|---|---|
| 5000 | 100.0 ms | 97.55 ms |
| 20000 | 25.0 ms | 21.55 ms |
| 30000 | 16.67 ms | 13.55 ms |

NotReady ≈ frame playtime, consistently ~3ms under ideal (device signals Ready
just before playback fully finishes). Idle = always Ready.

**Boundaries:** bulk writes return exactly `n*7+5` bytes; raw workaround sizes
(45→320B, 109→768B) do not stall the USB write; oversize 5000-pt short-writes
28864 B at the bulk timeout; writing while NotReady back-pressures the bulk
write (short-write / `Timeout`); pps footer is u16 little-endian, truncated for
values > 65535.
