# Ether Dream simulator

A virtual Ether Dream DAC with a GUI. It serves the `laser-dac` crate's Ether Dream simulator model on the real ports. Streaming uses TCP 7765, and discovery broadcasts go to UDP 7654. Any Ether Dream client on the network sees it as hardware, including this crate's discoverer.

The window draws the points the simulated DAC plays. The side panel switches the firmware profile and injects faults while a client is connected. It also triggers an e-stop, opens the interlock, and shows the live status.

```sh
cargo run -p etherdream-simulator --release
cargo run -p etherdream-simulator --release -- --profile ed1-j4cdac
cargo run -p etherdream-simulator --release -- --list-profiles

# Loopback only, no broadcasts:
cargo run -p etherdream-simulator --release -- --bind 127.0.0.1:7765 --no-broadcast
```

Switching the profile restarts the server, so connected clients see their connection drop, as they would on a power cycle.

Do not run it on a network that also has a real Ether Dream. Clients cannot tell the two apart except by MAC address. The simulator uses locally administered MACs that start with `02:ed`.
