# AGENTS.md — esp32-mqtt-blink

Single-binary ESP32 firmware (Rust, `esp-idf-svc` std stack) that blinks an LED at a speed controlled over MQTT via 0–10 level.

## Build & flash

```bash
# Prerequisites (one-time):
cargo install espup
espup install
. $HOME/export-esp.sh
cargo install espflash ldproxy

# Build and flash (Xtensa env must be sourced first):
cargo build --release
espflash flash --monitor target/xtensa-esp32-espidf/release/esp32-mqtt-blink
```

- `rust-toolchain.toml` pins `channel = "esp"` — plain `stable` rustc lacks Xtensa codegen.
- `.cargo/config.toml` pins `target = "xtensa-esp32-espidf"` and `linker = "ldproxy"`.
- If build fails with `can't find crate for core` or `Unsupported target`, the Xtensa env is not sourced.

## Required setup

`cfg.toml` (gitignored, copy `cfg.toml.example`) must exist with real `wifi_ssid` / `wifi_pass` / `mqtt_url` **at compile time** — `#[toml_cfg::toml_config]` bakes values into the binary. Empty `wifi_ssid` triggers a deliberate `anyhow::bail!` at startup.

## What exists / what doesn't

- **No test suite, lint, or CI.** Verification is flash-and-observe.
- **No serde, no async/embassy.** JSON status payloads are hand-formatted strings. `embassy-time-driver` feature on `esp-idf-svc` will link-fail (no executor).
- The reusable `esp32-rust-idf` skill is at `.claude/skills/esp32-rust-idf/SKILL.md` — load it for toolchain or MQTT runtime issues.
- `DEVELOPMENT_JOURNEY.md` documents every gotcha hit during bring-up.

## Architecture

All code in `src/main.rs` — one file, no modules. Five threads in `main()`:

1. **Connection-draining thread** — pumps `connection.next()` continuously. **Never** call `subscribe`/`publish`/`enqueue` from here; it deadlocks the client.
2. **Subscriber thread** — retries `subscribe()` with 500ms backoff. First attempt **expected** to fail with `ESP_FAIL` (pre-connection handshake).
3. **Status-publisher thread** — receives `(level, reason)` via `mpsc::channel` from thread #1, calls blocking `publish()`. This indirection keeps blocking calls off the draining thread.
4. **Heartbeat thread** — republishes current level every 30s.
5. **Main / blink loop** — drives `gpio2` (onboard LED) using `FreeRtos::delay_ms` and shared `AtomicU32`. Level 0 holds LED low and polls at 100ms.

Key compile-time constructs:
- `DELAY_TABLE` \[Option\<u32\>; 11\] built by `const fn` using `while` (not `for` — not stable in const context). Don't "simplify" this.
- `build_delay_table()` linearly maps level 1–10 to 1000–50ms delay.

Config (`*const` feature) and MQTT status (`retain: false`, no LWT) are known limitations — don't silently patch unless asked.

## Crate versioning

If bumping `esp-idf-svc`/`esp-idf-sys`/`esp-idf-hal`, also update `embuild` in `[build-dependencies]` so feature unification enables `embuild::espidf::sysenv`. Stale versions cause `cannot find espidf in embuild` or `c_char` signedness errors.

See `CLAUDE.md` for the full architecture breakdown (preserved alongside this file), and
`INTENT.md` for why the project exists, its non-goals, and the invariants any change should
preserve.
