# esp32-mqtt-blink

ESP32 firmware (Rust, `esp-idf-svc` std stack) that drives an onboard LED over MQTT. Designed to
be controlled by an AI agent ("Hermes") shelling out to `mosquitto_pub`/similar from another
machine on the same LAN, though any MQTT client works the same way.

Two independent commands, on two separate topics:

- **`blink <0-10>`** — a speed level (not raw milliseconds):
  - Level `0` → LED off
  - Level `1` → slowest blink
  - Level `10` → fastest blink
  - Delay-per-level is precomputed at **compile time** into a const lookup
    table (`DELAY_TABLE`) — no runtime calculation, no heap allocation.
- **`switch on|off|toggle`** — a separate on/off gate that **completely bypasses the blink timing
  loop**: `on` holds the LED high, `off` holds it low, neither one "resumes blinking." `toggle`
  treats a prior `blink` command as "currently on," so `blink 5` → `switch toggle` deterministically
  turns the LED off.

On boot, the firmware also picks a sensible default before any command arrives: solid on once
connected to the broker, or fast-blinking (`blink 10`) if it can't reach MQTT within 10s. See
`IMPLEMENTATION_PLAN.md` for the full design rationale, and `INTENT.md` for the project's purpose,
non-goals, and the invariants any change should preserve.

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
   topic_speed = "esp32/speed_level"
   topic_switch = "esp32/switch"
   topic_status = "esp32/status"
   ```

   `cfg.toml` is gitignored — your credentials never get committed.
   If `wifi_ssid` is left empty, the firmware fails fast at startup with a
   clear error instead of silently looping.

   Prefer a `.local` mDNS hostname over a literal IP for `mqtt_url` (e.g.
   `mqtt://your-broker-host.local:1883`) if your broker runs on a machine that gets its
   address from DHCP. ESP-IDF's lwIP resolves `.local` names via a one-shot mDNS query on
   every connect/reconnect attempt, so the device keeps working after the broker's IP
   changes (e.g. a router restart) with no rebuild or reflash — a literal IP breaks
   silently the next time DHCP reassigns it.

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

### `blink` — speed level

Publish a level 0–10 to the speed topic:

```bash
mosquitto_pub -h broker.example.com -t esp32/speed_level -m 10   # fastest
mosquitto_pub -h broker.example.com -t esp32/speed_level -m 1    # slowest
mosquitto_pub -h broker.example.com -t esp32/speed_level -m 0    # off (within blink mode)
```

### `switch` — on/off gate, bypasses the blink loop entirely

```bash
mosquitto_pub -h broker.example.com -t esp32/switch -m "on"      # LED held high, blink loop skipped entirely
mosquitto_pub -h broker.example.com -t esp32/switch -m "off"     # LED held low, blink loop skipped entirely
mosquitto_pub -h broker.example.com -t esp32/switch -m "toggle"  # flips on/off; a prior blink command counts as "on"
```

### Status

Watch device status (published on every command, on rejection, on the boot-time defaults below,
and every 30s as a heartbeat):

```bash
mosquitto_sub -h broker.example.com -t esp32/status
```

`level`/`delay_ms` are `null` whenever `mode` isn't `"blink"`, since neither describes anything
actually driving the LED while it's held statically on/off:

```json
{"mode":"blink","level":7,"delay_ms":367,"reason":"updated"}
```
```json
{"mode":"on","level":null,"delay_ms":null,"reason":"switch_on"}
```
```json
{"mode":"off","level":null,"delay_ms":null,"reason":"switch_off"}
```

### Boot-time defaults

Before any command arrives from the network, the firmware picks a state on its own:

- **Can't reach the broker within 10s of WiFi connecting** → `blink 10` (fast-blink distress
  signal), `reason:"offline_fallback"`.
- **Successfully subscribes, no command received yet** → `switch on` (solid light = "alive,
  connected, idle"), `reason:"connected_default"`. This still applies even if the offline
  fallback already fired first — a broker that comes up late correctly overrides the fast-blink
  signal with a solid one.

Both defaults are one-time boot checks — verified end-to-end on real hardware, see
`IMPLEMENTATION_PLAN.md`'s test log for the exact timing observed.

## Project layout

```
esp32-mqtt-blink/
├── .cargo/config.toml           # pins target = xtensa-esp32-espidf, linker = ldproxy
├── rust-toolchain.toml          # pins the `esp` rustup toolchain
├── Cargo.toml
├── build.rs
├── cfg.toml.example              # committed template, no real values
├── cfg.toml                      # your real secrets — gitignored, fill in after cloning
├── CLAUDE.md                     # architecture notes for AI coding assistants
├── INTENT.md                     # why this project exists: goals, non-goals, invariants
├── DEVELOPMENT_JOURNEY.md        # issues hit + fixes while bringing this up on hardware
├── IMPLEMENTATION_PLAN.md        # switch command design rationale + hardware test log
├── .gitignore
└── src/main.rs
```

The reusable ESP32 Rust toolchain/MQTT playbook lives in the `esp32-rust-idf` Claude Code skill,
available globally at `~/.claude/skills/` rather than checked into this repo.

## Notes / next steps

- **Secrets are still embedded in the compiled binary.** `cfg.toml` keeps
  them out of git, but anyone with flash/physical access to the device can
  extract them from the firmware image. For stronger isolation, move to
  NVS-based provisioning per device instead of compile-time config.
- **Non-linear level mapping**: levels are currently linearly spaced in
  delay (ms), so the perceived speed change is more dramatic at the fast
  end. Swap `build_delay_table()` for an exponential curve if you want each
  level to feel like an equal visual step.
- **Broker address survives DHCP changes** by using a `.local` mDNS hostname in `mqtt_url`
  instead of a literal IP (see Setup above) — this was previously a real problem (a router
  restart reassigning the broker host's IP silently broke the connection until a
  rebuild+reflash) and is now handled without any firmware code changes, since ESP-IDF's
  lwIP already resolves `.local` names fresh on every connect/reconnect. Still assumes mDNS
  multicast actually reaches the ESP32 over your specific WiFi AP — if that's ever not true
  on a given network, fall back to a static DHCP reservation for the broker host instead.
- **Retained MQTT status / LWT**: consider publishing status with
  `retain: true` so a newly-subscribed client immediately sees the last
  known state, and registering a Last Will and Testament message so the
  broker can report the device going offline uncleanly.
