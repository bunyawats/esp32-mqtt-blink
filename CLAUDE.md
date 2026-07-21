# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Single-binary ESP32 firmware (Rust, `esp-idf-svc` std stack) that blinks an onboard LED at a
speed controlled over MQTT via a 0–10 speed level (not raw milliseconds), instead of hardcoding
a blink rate or requiring raw millisecond values over the wire. The entire firmware lives in
`src/main.rs` — there is no library crate or module split. It's been built, flashed, and verified
end-to-end against a real ESP32 board and a real Mosquitto broker (see `DEVELOPMENT_JOURNEY.md`
for the full list of toolchain and runtime issues hit and fixed along the way — several are
generic ESP32/Rust pitfalls worth knowing before touching this stack again, and are also captured
in the reusable `esp32-rust-idf` Claude Code skill).

## Prerequisites (one-time host setup)

```bash
cargo install espup
espup install
. $HOME/export-esp.sh   # source the generated env file (path per espup's output)
cargo install espflash ldproxy
```

The Xtensa toolchain env must be sourced in the shell before `cargo build` will work — if a build
fails with missing Xtensa target/toolchain errors, check that `export-esp.sh` was sourced.

Three project files pin the toolchain/target so a plain `cargo build` does the right thing instead
of silently building for the host:
- `rust-toolchain.toml` pins `channel = "esp"` — the plain `stable` rustc from rustup has no Xtensa
  codegen support at all; only espup's `esp` toolchain does. Without this file, cargo silently uses
  `stable` and fails deep in dependency compilation with `can't find crate for core`.
- `.cargo/config.toml` sets `target = "xtensa-esp32-espidf"` and `linker = "ldproxy"`. Without it,
  cargo defaults to the host target and `esp-idf-sys`'s build script rejects it outright
  (`Error: Unsupported target 'aarch64-apple-darwin'` or similar).
- The `esp-idf-*` crate versions in `Cargo.toml` must stay reasonably current — older releases
  (e.g. `esp-idf-svc` 0.49 / `esp-idf-sys` 0.35) assume `c_char` is signed, which breaks against
  newer Xtensa toolchains that generate bindings with unsigned `c_char`
  ([esp-rs/esp-idf-sys#375](https://github.com/esp-rs/esp-idf-sys/issues/375)). If bumping these,
  `embuild` in `[build-dependencies]` must track a compatible version too, or Cargo's feature
  unification won't enable the `espidf` feature `build.rs` needs (`embuild::espidf::sysenv`), and
  the build fails with `cannot find espidf in embuild`.

Do **not** add the `embassy-time-driver` feature to `esp-idf-svc` unless the firmware actually
starts using `embassy`/async — nothing in `main.rs` does today (it's blocking/thread-based
throughout). That feature pulls in `embassy-time-driver`'s generic-queue architecture, which needs
a real embassy executor to provide `__embassy_time_queue_item_from_waker`; without one, it fails
at link time, not compile time.

## Commands

```bash
cargo build --release                                                    # build firmware
espflash flash --monitor target/xtensa-esp32-espidf/release/esp32-mqtt-blink  # flash + serial monitor
```

There is no test suite, lint config, or CI — this is embedded firmware flashed to real hardware;
verification is done by flashing and observing MQTT/LED behavior, not `cargo test`.

Target triple depends on the `esp-idf-svc`/`espup` version in use; check `cargo build` output if
the path above doesn't match what's on disk.

## Required local setup before building

`cfg.toml` (gitignored) must exist, copied from `cfg.toml.example`, with real `wifi_ssid` /
`wifi_pass` / `mqtt_url` filled in. If `wifi_ssid` is empty, the firmware intentionally fails fast
at startup (`anyhow::bail!`) rather than silently retrying — this is a deliberate check in
`main()`, not a bug.

## Architecture

Config is loaded via `toml-cfg`'s `#[toml_cfg::toml_config]` macro on the `Config` struct in
`main.rs`, which reads `cfg.toml` **at compile time** and bakes values into the binary as `&'static
str` constants (accessed via `CONFIG`). This means:
- Changing `cfg.toml` requires a rebuild, not just a reflash of existing artifacts.
- Secrets end up embedded in the compiled binary — anyone with physical/flash access to the
  device can extract them. This is a known limitation (see README "Notes / next steps"), not
  something to silently "fix" by moving to runtime config unless asked.

Speed-to-delay mapping is precomputed at **compile time** into `DELAY_TABLE`, a `const fn`-built
lookup array (`build_delay_table()`), indexed by level 0..=10. This avoids runtime float math /
heap allocation on the microcontroller. `const fn` can't use `for` loops on stable Rust, so the
table-building loop deliberately uses `while` — don't "simplify" that to a `for` loop, it won't
compile in const context. The mapping is currently linear (see README for the exponential-curve
alternative under consideration).

Runtime shape (all in `main()`): WiFi (`BlockingWifi`, connects using `cfg.toml` credentials
before anything else runs) + four threads sharing state via `Arc<AtomicU32>` (speed level) and
`Arc<Mutex<EspMqttClient>>` (MQTT client), plus the main thread's blink loop:

- **Connection-draining thread**: does nothing but `while let Ok(event) = connection.next()`,
  reacting only to `Received` messages (parse as `u32`, validate against `MAX_LEVEL`, update the
  shared level, send a `(level, reason)` tuple on the status channel). This thread must never
  block on anything else — pumping `connection.next()` is what drives the MQTT client's internal
  connect/reconnect state machine forward. **Never** call a blocking client method (`subscribe`,
  `publish`, `enqueue`) from this thread; that deadlocks it (see DEVELOPMENT_JOURNEY.md #9) since
  it would then stop draining events, and the client can't progress internally.
- **Subscriber thread**: calls `subscribe(topic_speed, ...)` in a retry loop (500ms backoff)
  independent of any event. The first attempt is *expected* to fail with `ESP_FAIL` before the
  connection handshake completes — that's normal (see DEVELOPMENT_JOURNEY.md #8), not a bug to
  silence. Must run on a separate thread from the draining thread above.
- **Status-publisher thread**: the only thread that actually calls `publish_status()` in response
  to incoming commands; receives `(level, reason)` off an `mpsc::channel` fed by the draining
  thread. This indirection exists solely to keep blocking publish calls off the draining thread.
- **Heartbeat thread**: republishes current status every `HEARTBEAT_SECS` (30s) regardless of
  whether the level changed (calls `publish_status()` directly — it's already off the draining
  thread, so no channel indirection needed here).
- **Main thread**: drives the LED GPIO pin (`gpio2` — the onboard LED on many ESP32 dev boards;
  change `peripherals.pins.gpio2` if the target board wires it elsewhere) in a blocking on/off
  loop using `FreeRtos::delay_ms`, polling the shared level each cycle. Level 0 doesn't stop the
  loop — it holds the LED low and polls at `OFF_POLL_MS` so a new command is picked up quickly.

Status payloads are hand-formatted JSON strings (`{"level":N,"delay_ms":N|null,"reason":"..."}`),
not a serde model — there's no JSON library dependency in `Cargo.toml`. Keep new fields consistent
with this hand-rolled format unless a real need for serde_json arises (it adds binary size on a
flash-constrained target).

MQTT status is currently **not** retained and there's no LWT (Last Will and Testament) configured
— a newly-subscribed client won't see last-known state until the next heartbeat, and the broker
won't report unclean disconnects. This is a known gap, not an oversight to silently patch.