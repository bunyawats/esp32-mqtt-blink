# esp32-mqtt-blink

ESP32 firmware (Rust, `esp-idf-svc` std stack) that blinks an LED at a speed
controlled over MQTT, using a simple 0–10 speed level instead of raw
milliseconds.

- Level `0` → LED off
- Level `1` → slowest blink
- Level `10` → fastest blink
- Delay-per-level is precomputed at **compile time** into a const lookup
  table (`DELAY_TABLE`) — no runtime calculation, no heap allocation.

## Prerequisites

```bash
cargo install espup
espup install
# Source the generated env file as instructed by espup, e.g.:
. $HOME/export-esp.sh

cargo install espflash ldproxy
```

`rust-toolchain.toml` and `.cargo/config.toml` are committed in this repo and pin the
`esp` rustup toolchain and `xtensa-esp32-espidf` target, so `cargo build` targets the
ESP32 correctly out of the box — no manual toolchain/target setup beyond the above.

You'll also need an MQTT broker reachable from the device (e.g. Mosquitto,
HiveMQ, or a cloud broker) and its address. If you're running Mosquitto locally for
testing, make sure it's listening on your LAN interface, not just loopback — see
`DEVELOPMENT_JOURNEY.md` (#7) if the device can't reach it.

## Setup

1. Copy the config template and fill in your details:

   ```bash
   cp cfg.toml.example cfg.toml
   ```

   Edit `cfg.toml`:

   ```toml
   [esp32-mqtt-blink]
   wifi_ssid = "YourWiFiName"
   wifi_pass = "YourWiFiPassword"
   mqtt_url = "mqtt://broker.example.com:1883"
   mqtt_client_id = "esp32-blinker"
   topic_speed = "esp32/blink/speed_level"
   topic_status = "esp32/blink/status"
   ```

   `cfg.toml` is gitignored — your credentials never get committed.
   If `wifi_ssid` is left empty, the firmware fails fast at startup with a
   clear error instead of silently looping.

2. Check the LED pin. The code assumes `gpio2` (the onboard LED on many
   ESP32 dev boards). Update `peripherals.pins.gpio2` in `src/main.rs` if
   your board wires the LED elsewhere.

## Build & flash

```bash
cargo build --release
espflash flash --monitor target/xtensa-esp32-espidf/release/esp32-mqtt-blink
```

(Exact target triple depends on your `esp-idf-svc` / `espup` version — check
`cargo build` output if the path above doesn't match.)

This has been built, flashed, and verified end-to-end against real hardware and a real
MQTT broker. If you hit a build or runtime error along the way, check
`DEVELOPMENT_JOURNEY.md` first — it covers the toolchain and MQTT-client issues most
likely to come up (target/toolchain mismatches, `c_char` signedness errors, MQTT
subscribe races, broker connectivity) with root causes and fixes.

## Usage

Publish a level 0–10 to the speed topic:

```bash
mosquitto_pub -h broker.example.com -t esp32/blink/speed_level -m 10   # fastest
mosquitto_pub -h broker.example.com -t esp32/blink/speed_level -m 1    # slowest
mosquitto_pub -h broker.example.com -t esp32/blink/speed_level -m 0    # off
```

Watch device status (published on update, on rejection, and every 30s as a
heartbeat):

```bash
mosquitto_sub -h broker.example.com -t esp32/blink/status
```

Example payload: `{"level":7,"delay_ms":367,"reason":"updated"}`

## Project layout

```
esp32-mqtt-blink/
├── .cargo/config.toml           # pins target = xtensa-esp32-espidf, linker = ldproxy
├── .claude/skills/esp32-rust-idf/SKILL.md  # reusable ESP32 Rust toolchain/MQTT playbook
├── rust-toolchain.toml          # pins the `esp` rustup toolchain
├── Cargo.toml
├── build.rs
├── cfg.toml.example              # committed template, no real values
├── cfg.toml                      # your real secrets — gitignored, fill in after cloning
├── CLAUDE.md                     # architecture notes for AI coding assistants
├── DEVELOPMENT_JOURNEY.md        # issues hit + fixes while bringing this up on hardware
├── .gitignore
└── src/main.rs
```

## Notes / next steps

- **Secrets are still embedded in the compiled binary.** `cfg.toml` keeps
  them out of git, but anyone with flash/physical access to the device can
  extract them from the firmware image. For stronger isolation, move to
  NVS-based provisioning per device instead of compile-time config.
- **Non-linear level mapping**: levels are currently linearly spaced in
  delay (ms), so the perceived speed change is more dramatic at the fast
  end. Swap `build_delay_table()` for an exponential curve if you want each
  level to feel like an equal visual step.
- **Retained MQTT status / LWT**: consider publishing status with
  `retain: true` so a newly-subscribed client immediately sees the last
  known state, and registering a Last Will and Testament message so the
  broker can report the device going offline uncleanly.
