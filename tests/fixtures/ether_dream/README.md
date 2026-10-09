# Ether Dream wire captures

These files record request and reply traffic between a host and a real Ether Dream DAC. The replay tests in `src/protocols/ether_dream/replay.rs` read them. Those tests check that the simulator model in `src/protocols/ether_dream/sim/model.rs` reproduces each reply, and that the `ed2-r331` firmware profile matches what the hardware advertised.

All captures sent blanked points only. Every point field was zero, the laser stayed dark, and the galvos stayed centred.

## Files

| File | Source | Events | Content |
|---|---|---|---|
| `ed2_r331_identity.jsonl` | raw bytes | 3 | The hello status, a ping, and the `'v'` build string. |
| `ed2_r331_lifecycle.jsonl` | transcribed | 36 | Prepare, data, begin, underflow, stop, e-stop and clear. Also covers commands sent in the wrong state, and ends with an unknown command that resets the connection. |
| `ed2_r331_capacity.jsonl` | transcribed | 54 | Fills to 3871 points, a second concurrent connection, queued rate changes, and drain timing. |
| `ed2_r331_hw_identity.jsonl` | raw bytes | 4 | The hello, a ping, the `'v'` build string, and a ping showing the connection is still in sync. |
| `ed2_r331_hw_lifecycle.jsonl` | raw bytes | 26 | Drain to underflow, data and begin while idle, update, queued rate, stop twice, data while idle, e-stop, clear and prepare. |
| `ed2_r331_hw_capacity.jsonl` | raw bytes | 9 | Fills to exactly the advertised 3899 points, a rejected write past that, then 55 ms of playback at 1000 points per second. |
| `ed2_r331_hw_drain.jsonl` | raw bytes | 45 | A 30 000 points per second drain sampled every 3 ms down to underflow, then a refill loop that falls behind and underflows mid-stream. |
| `ed2_r331_hw_broadcast.jsonl` | raw bytes | 1 | One UDP broadcast, heard right after the `hw` captures. |
| `ed2_r331_hwcheck_*.jsonl` | raw bytes | 1 to 550 | What the production backend sent and the DAC answered, one file per scenario. See "Backend checks on hardware" below. |

The DAC was an Ether Dream 2 with build string `r331-ed4bef5`. Its broadcast reports hardware revision 10 and software revision 2. The broadcast file zeroes the device-specific half of its MAC address and keeps the vendor prefix.

The first three files were captured with the DAC at the link-local address `169.254.102.149`. The DAC's UDP broadcast never reached that host. After a later power cycle the same DAC came up at `192.168.254.66`, on a host interface with a static `192.168.254.1/24` address. The `hw` files were recorded there with `examples/ether_dream_capture.rs`, with no other client connected. The broadcast was received on that interface.

**Raw bytes versus transcribed.** The identity file and the `hw` files hold the reply bytes exactly as they came off the socket. Requests keep only the command header, with zeroed point payloads elided. The lifecycle and capacity files were rebuilt from a probe script's text log, which printed the decoded status fields. Every decoded field was turned back into bytes, so the replies are byte-accurate for the fields that were logged. These assumptions apply:

- `dur_us` is `null`. The probe did not time each exchange separately.
- `t_us` and `dt_us` are rebuilt from the probe's sleeps and log timestamps, accurate to about a millisecond.
- In the capacity file, the source and status-flag fields were not logged. They are assumed to be 0.

## Format

One JSON object per line.

| Field | Meaning |
|---|---|
| `seq` | 1-based event index. |
| `phase` | `connect`, `pre`, `stream` or `reconnect` for the hardware files. The capture example uses the scenario name instead. |
| `op` | The command name, e.g. `hello`, `ping`, `prepare`, `data`, `begin`, `stop`, `estop`. |
| `req` | Request bytes as hex. For data commands this is only the 3-byte header. The payload was all zeros and is padded back in on replay. |
| `resp` | Reply bytes as hex. It is 22 bytes for a status reply, 32 bytes for `'v'` and 36 bytes for a broadcast. It is empty on a timeout or reset. |
| `status` | `ok`, `timeout`, `reset` or `closed`. The hwcheck files add `blocked` for a command the capture proxy refused to forward, and `synthesized` for a reply the proxy made up without contacting the DAC. Replay skips both. |
| `n` | Total request length in bytes, including any elided payload. |
| `dt_us` | Microseconds since the previous event ended. |
| `dur_us` | Microseconds from sending the request to the end of the reply, or `null` when unknown. Replay feeds each command to the model at the middle of this span. |
| `t_us` | Microseconds from the start of the capture to the end of this event. |
| `note` | Free text. A note starting with `UNEXPLAINED` marks an event the model is not expected to match. |

## What the hardware showed

- The broadcast advertises a capacity of 3899 points and a maximum rate of 100 000 points per second. The ring holds exactly 3899 points. A further 100-point write got NAK-Invalid with fullness unchanged. The DAC never sent NAK-Full.
- Status flags are `0x5f31` when idle and `0x5f33` after an underflow. During e-stop they are `0x5f35`, with light-engine state 3 and light-engine flags `0x3`. After the power cycle the same bits read `0x7731`, `0x7733` and `0x7735`.
- An underflow leaves playback idle with fullness, rate and point count all 0. The underflow flag stays set until the next prepare.
- After clear-e-stop the light engine is Ready with flags 0, but the e-stop playback flag stays set until the next prepare.
- `'u'` changes the rate at once. A `'q'` rate is queued without changing the reported rate.
- Fullness from the last session is still reported after stop. It clears only on the next prepare.
- `begin` while idle is ACKed and ignored. `stop` and data while idle get NAK-Invalid.
- An unknown command, such as `'z'`, resets the TCP connection. `0xff` is an alias for e-stop. Clear-e-stop returns to Ready immediately.
- A second TCP connection is accepted. The DAC sends nothing it was not asked for.
- All TCP clients share one playback state. A prepare, stop or e-stop from a second client acts on the stream the first client is playing.
- A rate queued with `'q'` is not applied unless a point carries the rate-change control bit.
- The `'v'` reply is exactly 32 raw bytes: the build string `r331-ed4bef5` padded with NULs. Unlike every other reply, it has no response byte and no command echo in front. The identity file holds it verbatim.
- A `'b'`, `'u'` or `'q'` whose rate is above the maximum gets NAK-Invalid after the DAC consumes only the opcode byte. The DAC then parses the argument bytes as further commands. In a probe the arguments were all `'?'` bytes, and the DAC answered with one extra ping reply per argument byte: six after `'b'`, four after `'q'` and six after `'u'`. By the same rule, a low-water mark of 0 would be read as two `0x00` bytes, which are e-stops. These replies are not in a fixture file.
- A `'b'` with rate 0 hung the firmware. It stopped answering and needed a power cycle.
- The undocumented upper byte of the playback flags is not constant. The first fixtures show `0x5f`, but after a later power cycle the same DAC reported `0x77`, for example `0x7731` when idle. Clients must ignore that byte. Replay takes the idle flag word of each capture's boot.
- Closing the TCP connection while playing stops playback. The next hello shows playback idle with the leftover fullness frozen, for example 35 or 43 points, and no underflow bit. The DAC does not play its ring out. Seen in every hwcheck scenario that ended a stream without a stop.
- One data command carrying all 3899 points, 70 185 bytes, is ACKed with fullness 3899.
- With 50 points of room in a prepared ring, a 100-point data command stores the 50 that fit and gets NAK-Invalid. Fullness goes from 3849 to 3899. The rest of the payload is consumed, and the next reply is in sync. This is the ED1 behaviour the ED2 profile assumes.
- The DAC occasionally takes 20 to 35 ms to answer a small data command that normally takes under 1 ms. The status in that reply is then as old as the delay.
- Playback timing matches the point rate. At 1000 points per second, 55 points played in about 55 ms. At 30 000 points per second, about 92 points drained per 3.07 ms sample.

**Corrections to the first capture.** The first three files were read as showing a ring larger than the advertised 1799 points. That 1799 was never advertised. It was assumed from ED1 because no broadcast was heard. The first capacity file simply stopped filling at 3871, below the real 3899. Its stale idle fullness came from an earlier session, as did the 1400 points in the `hw` identity hello.

**Unexplained.** In the capacity file, 20 points that carried the control bit drained within about 6.6 ms at 1000 points per second. They should have taken 20 ms. The model does not reproduce this, and replay feeds the event without judging it.

## Backend checks on hardware

The `ed2_r331_hwcheck_*` files were recorded on 2026-10-04 by `examples/ether_dream_hwcheck.rs`, against the same DAC at `192.168.254.66` in the same boot as the `hw` files, with idle flags `0x7731`. Unlike the capture example, this tool drives the production `EtherDreamBackend`, either directly or through the presentation layer's `Stream`, which was never armed. The backend connects to an in-process TCP proxy that forwards to the DAC and records every exchange. The requests in these files are therefore exactly what the backend sent.

The proxy also acts as a safety interlock. It forwards only blanked centre points and rates from 1000 to 30 000 points per second with a low-water mark of 0. It never forwards an unknown opcode or a half-written command. While the DAC is in e-stop it forwards only ping and clear, and answers anything else itself with NAK-Invalid carrying the last real status. Before every connection it checks `lsof` and requires the hello to show playback idle. The tool aborts if `lsof` cannot run, and refuses to start if the advertised maximum rate is below 30 000 points per second. A scenario that triggers any interlock refusal fails, apart from the expected `'u'` block in `clamp_guard`.

| Scenario | Outcome | Key numbers |
|---|---|---|
| `broadcast_discovery` | PASS | Found on the `192.168.254.1` interface in 1.6 s. Both the scan result and the backend report 3899 points and 100 000 points per second. |
| `stale_full_reconnect` | PASS | The hello shows a stale 3899 points. The backend's first command after `'v'` is prepare, and the DAC plays 3.2 ms after the hello. |
| `underflow_recovery` | PASS | A 400 ms host pause at 1000 points per second. The next data write is NAKed on an idle DAC. Prepare, data and begin follow, and the DAC plays 1.5 ms later. No data was ACKed while idle. |
| `estop_recovery` | PASS | Two clears 1000 ms apart. The proxy held back one data and one prepare sent before the backend saw the e-stop. The DAC plays 1.8 ms after the second clear, on the same connection. The stream called the backend 6394 times during the e-stop, down from 2.6 million before the disarmed-spin fix. |
| `rate_change` | PASS twice | set_pps 1000, 20 000, 1000, 30 000, 1000. The backend sends each change as data then `'u'`, so every update reaches a playing DAC with 900 to 1500 points queued. The status rate follows within 17.5 ms, with no underflow. Before the fixes, 20 000 to 1000 starved the DAC for about 950 ms, and a raise sent with 33 points queued underflowed because `'u'` went ahead of the data. |
| `full_capacity_chunk` | PASS | 3899 points in one command are ACKed with fullness 3899. One more point gets NAK-Invalid with fullness 3899, the backend returns `WouldBlock`, and its estimate re-syncs to 3899. The next write finds the prepared ring with no room and sends `'b'`, and the DAC plays. |
| `clamp_guard` | PASS | The stream API refuses `'b'` 0, `'u'` 0, `'q'` 0 and `'b'` 200 000 with `InvalidInput` and sends nothing. Through the backend, pps 0 goes out as `'b'` 6250 and pps 200 000 as `'u'` 100 000. The proxy refused to send that update because it is above 30 000. |
| `partial_room` | PASS | 3849 of 3899 points prepared, then one 100-point write. NAK-Invalid with fullness 3899, then a ping shows 3899. |
| `session_stop` | PASS | `Stream::run` at 10 000 points per second for 2 s, then `SessionControl::stop()`. Exactly one `'s'` follows the `'b'`, ACKed with playback idle, then the backend disconnects. The next hello shows idle. |
| `steady_stream_60s` | PASS on latest run | 30 000 points per second for 60 s, four runs on the fixed backend. The latest had no underflow, one prepare, and a DAC minimum of 1158 points. Two earlier runs had no underflow but one stale 27 or 35 ms DAC reply each, which put a single estimate about 1000 points high. One run underflowed twice after host stalls of 47 and 60 ms against the 50 ms buffer. The backend re-prepared within 5 ms each time. Excluding writes next to a slow round trip, the estimate stayed within 300 points. |

These runs used the old 50 ms default target buffer. The Ether Dream default is now 80 ms, capped at 80 % of the ring by the backend.

The steady file keeps only the first 3 s of the first 60 s run. `ed2_r331_hwcheck_steady_underflow_excerpt.jsonl` holds the 70 events around that run's underflow, after an 87 ms host stall. `ed2_r331_hwcheck_rate_change_before_fix.jsonl` is the confirming run of the downward-rate bug, and `ed2_r331_hwcheck_rate_up_before_fix.jsonl` is the first 1.7 s of a run that underflowed on a raise before the backend sent data ahead of `'u'`. The other files are from the latest runs against the fixed backend. Streams end with an ACKed `'s'` from the backend, except where the scenario is about something else: in `stale_full_reconnect` the backend's connection closes while playing, and in `full_capacity_chunk` the backend's own stop is NAKed because the proxy's cleanup stop already left the DAC idle. Replay feeds a command pipelined behind another no earlier than the previous reply, and keeps the old rate's fullness tolerance across a rate update until the next prepare. The tool also writes a `.writes.jsonl` file per scenario, logging every `try_write_points` call with the estimate beforehand. Those files are not committed.

```sh
cargo run --example ether_dream_hwcheck --features testutils,ether-dream -- \
    --addr 192.168.254.66:7765 --out /tmp/ed-hwcheck --scenarios stale_full_reconnect
cargo run --example ether_dream_hwcheck --features testutils,ether-dream -- \
    --sim ed2-r331 --out /tmp/ed-hwcheck-sim --steady-secs 3
```

## Untested

These were deliberately not probed, because a firmware hang has no watchdog and needs a power cycle:

- `'u'` with rate 0. It is expected to hang like `'b'` with rate 0.
- `'q'` with rate 0.
- Data sent during e-stop.
- A command left half-written for a long time. An earlier probe suggests the DAC waits forever for the rest.
- Rates above the maximum combined with argument bytes other than `'?'`, which could decode as e-stop or as data.
- A low-water mark other than 0. The field is documented as unused.

## Second capture attempt

Before the `hw` files were recorded, the capture tool ran once against `192.168.254.66` while another application was streaming to the DAC over its own TCP connection at the same time. Because the two clients share one playback state, the capture interleaved with that stream and its fullness and point counts include the other client's points. That capture was not added here. The capture tool now refuses to start unless every connection's hello shows playback idle.

## Recording a new capture

Use the capture example. It only sends blanked points, never sends a rate of 0, and never exceeds the advertised maximum rate. It sends only command sequences already probed on ED2 hardware. Close every other client of the DAC first, because the tool aborts unless the hello shows playback idle and the light engine ready.

```sh
cargo run --example ether_dream_capture --features testutils,ether-dream -- \
    --addr 192.168.254.66:7765 --out tests/fixtures/ether_dream --prefix ed2_r331_hw

# Only record the UDP broadcast, without opening a TCP connection:
cargo run --example ether_dream_capture --features testutils,ether-dream -- \
    --addr 192.168.254.66:7765 --listen-only --out tests/fixtures/ether_dream --prefix ed2_r331_hw

# Self-test the tool against the simulator:
cargo run --example ether_dream_capture --features testutils,ether-dream -- \
    --sim ed2-r331 --out /tmp/ed-capture
```

It writes five files: identity, lifecycle, capacity, drain and broadcast. It takes the capacity and maximum rate from the DAC's broadcast and exits with code 2 if none is heard within 3 s, so connect the host to an interface that receives it. Zero the device-specific half of the MAC address before committing a broadcast file. Its output uses the format above, with `dur_us` filled in. Pass `--include-reset` to end the lifecycle scenario with an unknown command.

To support another firmware, capture it this way, add a profile with `Provenance::HardwareVerified`, and add replay tests for it.
