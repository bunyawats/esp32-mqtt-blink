# Development journey: getting this firmware running on real hardware

This log captures every issue hit while taking this project from "code exists"
to "flashed on an ESP32 and controllable over MQTT", in the order they were
found. Kept for future reference — several of these are generic ESP32/Rust
pitfalls, not specific to this project (see the `esp32-rust-idf` skill, which
generalizes the toolchain-level issues below for reuse in other projects).

## 1. Toolchain wasn't installed at all

`espup`, `espflash`, and `ldproxy` weren't on `PATH`, and there was no `esp`
rustup toolchain — only plain `stable-aarch64-apple-darwin`.

Fix: `cargo install espup && espup install && cargo install espflash ldproxy`,
then source the generated `export-esp.sh` in every new shell.

## 2. `cargo build` silently targeted the host, not the ESP32

With no `.cargo/config.toml`, cargo defaulted to the host target
(`aarch64-apple-darwin`), and `esp-idf-sys`'s build script rejected it outright:
`Error: Unsupported target 'aarch64-apple-darwin'`.

Fix: added `.cargo/config.toml` with `target = "xtensa-esp32-espidf"`,
`linker = "ldproxy"`, and `ESP_IDF_VERSION = "v5.2.2"`.

## 3. Plain `stable` rustc has no Xtensa codegen support

Even with the target set, the build failed with `can't find crate for core` —
`stable` rustc doesn't support Xtensa at all; only espup's separate `esp`
rustup toolchain does (`rustc 1.95.0-nightly ... (1.95.0.0)`), and nothing
pinned it.

Fix: added `rust-toolchain.toml`:
```toml
[toolchain]
channel = "esp"
```

## 4. `c_char` signedness mismatch between `esp-idf-svc` and the newer toolchain

With the `esp` toolchain finally in use, the build got much further, then
failed compiling `esp-idf-svc` itself:
```
expected `*const u8`, found `*const i8`
```
This is a known ecosystem issue
([esp-rs/esp-idf-sys#375](https://github.com/esp-rs/esp-idf-sys/issues/375)):
older `esp-idf-svc`/`esp-idf-sys` releases assumed a signed `c_char`; newer
Xtensa toolchains generate bindgen output with unsigned `c_char`. Fixed
upstream in later crate releases, not by any local workaround.

Fix: bumped `esp-idf-svc` 0.49→0.52, `esp-idf-hal` 0.44→0.46, `esp-idf-sys`
0.35→0.37. This in turn required bumping the `embuild` build-dependency
0.32→0.33 — otherwise Cargo's feature unification stops enabling the
`espidf` feature on `embuild` that `build.rs` needs
(`embuild::espidf::sysenv::output()`), failing with
`cannot find espidf in embuild`.

## 5. Unused `embassy-time-driver` feature broke linking

After the version bump, compilation succeeded but linking failed:
```
undefined reference to `__embassy_time_queue_item_from_waker'
```
Nothing in `main.rs` uses `embassy` or `async` — the firmware is entirely
blocking/thread-based. The `embassy-time-driver` feature on `esp-idf-svc` was
dead weight, and the newer `embassy-time-driver` it pulled in needs a real
embassy executor to provide that symbol.

Fix: removed the feature — `esp-idf-svc = { version = "0.52", features =
["critical-section"] }`.

At this point `cargo build --release` produced a valid Xtensa ELF. Flashing
with `espflash flash` also succeeded immediately.

## 6. `subscribe()` raced the async MQTT connect, causing a panic/reboot loop

First real boot on hardware: WiFi connected fine, then:
```
E (4396) mqtt_client: Client has not connected
thread '<unnamed>' panicked at src/main.rs:125:18: failed to subscribe: ESP_FAIL
```
`EspMqttClient::new()` returns immediately; the actual connect handshake
happens asynchronously via the `connection` event loop. The original code
called `.subscribe()` right after construction, before that handshake had a
chance to complete — an inherent race, not something that only shows up on a
flaky network.

First fix attempt: move `subscribe()` into the event loop, gated on receiving
an `EventPayload::Connected` event. This compiles and *looks* correct, but
turned out to be unreliable in practice (see #8).

## 7. MQTT broker unreachable — two separate environment issues

Once the crash was fixed, the client just kept retrying forever:
```
E (14416) esp-tls: [sock=54] select() timeout
E (14416) mqtt_client: Error transport connect
```
Root causes, both on the Mac mini running the broker:
- **Mosquitto (via Homebrew) was bound to loopback only** — `mosquitto 2.x`
  defaults to listening only on `127.0.0.1`/`::1` unless a `listener` is
  explicitly configured, even with an otherwise-default `mosquitto.conf`.
  There was already a `mosquitto_local.conf` on disk with the right listener
  config, but it was never wired in (`include_dir` was commented out) — so it
  had zero effect.
- **`cfg.toml`'s `mqtt_url` pointed at the wrong IP** (`192.168.1.50`) — the
  Mac's actual LAN address was `192.168.1.34`.

Fix: appended `listener 1883 0.0.0.0` and `allow_anonymous true` directly to
`mosquitto.conf`, restarted the service (`brew services restart mosquitto`),
and corrected the IP in `cfg.toml`.

## 8. Event-gated `subscribe()` silently hung forever

With the broker reachable, the fix from #6 still didn't work: the device
logged `MQTT: connecting...` (`BeforeConnect`) once and then nothing else,
ever — no `Connected`, no error, no timeout. `lsof` on the Mac showed the
ESP32 *did* open a TCP connection to the broker, but no further application
log lines appeared.

Root cause, confirmed by comparing against
[esp-rs/esp-idf-svc#441](https://github.com/esp-rs/esp-idf-svc/issues/441):
gating `subscribe()` on a `Connected` event delivered through the same
`connection.next()` loop is not the supported pattern. The **correct** pattern
(from esp-idf-svc's own updated example) is to call `subscribe()` in an eager
retry loop (every ~500ms) **independent of any event**, running concurrently
with — but on a **different thread than** — the thread draining
`connection.next()`. The first subscribe attempt is *expected* to fail with
`ESP_FAIL` before the connection completes; that failure is what drives
retry, not a bug to fix.

Fix: split into two threads — one that only drains `connection.next()` and
reacts to `Received` events, and a separate one that loops calling
`subscribe()` every 500ms until it succeeds.

## 9. Deadlock: publishing status from inside the draining thread

With subscribe finally succeeding, incoming speed-level messages were
received and logged (`New speed level: N`) — but the corresponding MQTT
status message never arrived, and *no further events were processed after
the first message* (confirmed by adding a temporary log line for every event
type and watching it go silent).

Root cause: `publish_status()` calls the blocking `client.publish()`. The
original code called it directly from within the `Received` match arm — i.e.
from the same thread that must keep calling `connection.next()` to keep the
client's internal state machine moving. Publishing from inside that thread
deadlocks it: the thread blocks in `publish()` and never returns to drain
further events, so the connection appears to freeze after exactly one
message.

Fix: introduced an `mpsc::channel` — the draining thread only ever sends
`(level, reason)` over the channel; a separate dedicated thread owns the MQTT
client and does the actual `publish_status()` call. The heartbeat thread was
already on its own separate thread, so it didn't need this change.

## Verified working

WiFi connects, MQTT subscribes (after one expected failed attempt + retry),
and round-tripped every tested level (0, 1, 5, 6, 10, and an out-of-range 15)
through the real broker with correct `delay_ms` values from the compile-time
lookup table, e.g. `{"level":6,"delay_ms":473,"reason":"updated"}`.
