---
name: esp32-rust-idf
description: Set up, build, flash, and debug ESP32 firmware written in Rust with esp-idf-svc/esp-idf-hal (the espup/esp-idf-template std stack). Use whenever a project targets an ESP32/ESP32-Cx/Sx board with esp-idf-sys, esp-idf-hal, or esp-idf-svc as dependencies, or when cargo build fails with Xtensa/RISC-V target errors, c_char mismatches, or ESP-IDF linker errors.
---

# ESP32 Rust (esp-idf-svc) development

Playbook for the `std`-based ESP32 Rust stack (`esp-idf-sys` / `esp-idf-hal` /
`esp-idf-svc`, generated from `esp-idf-template`). Covers toolchain setup and
the specific failure modes that show up when the toolchain, crate versions,
and project config drift out of sync — each one below was hit and fixed in a
real project; see that project's `DEVELOPMENT_JOURNEY.md` for full transcripts
if you built this skill from there.

## One-time host setup

```bash
cargo install espup
espup install                    # downloads Xtensa Rust fork + ESP-IDF tools, several hundred MB+
. $HOME/export-esp.sh            # must be re-sourced in every new shell
cargo install espflash ldproxy
```

`espup install` creates a separate rustup toolchain named `esp`
(`rustc ...-nightly ... (x.y.0.0)`), completely distinct from `stable`. Plain
`stable` rustc has **no Xtensa codegen support at all** — it will list Xtensa
targets in `rustc --print target-list` (they're defined) but fail with
`can't find crate for core` when actually building, because the sysroot isn't
there. RISC-V ESP32 variants (C3/C6/H2 etc.) work with upstream `stable` in
principle, but projects are normally generated to use the `esp` toolchain
uniformly — check `rust-toolchain.toml` before assuming otherwise.

## Three files that must exist and agree with each other

A project generated from `esp-idf-template` has all three; if any is
missing or was hand-edited into inconsistency, the build breaks in ways that
don't obviously point back to the cause:

1. **`rust-toolchain.toml`**
   ```toml
   [toolchain]
   channel = "esp"
   ```
   Missing → cargo silently uses `stable`, fails deep in dependency
   compilation with `can't find crate for core` (not an obviously toolchain-related error).

2. **`.cargo/config.toml`**
   ```toml
   [build]
   target = "xtensa-esp32-espidf"   # or riscv32imc-esp-espidf / riscv32imac-esp-espidf for Cx chips

   [target.xtensa-esp32-espidf]
   linker = "ldproxy"

   [unstable]
   build-std = ["std", "panic_abort"]

   [env]
   MCU = "esp32"                    # must match the target above
   ESP_IDF_VERSION = "v5.2.2"
   ```
   Missing → cargo defaults to the host target, and `esp-idf-sys`'s build
   script rejects it outright: `Error: Unsupported target 'aarch64-apple-darwin'`
   (or whatever the host triple is). This is the #1 misleading error for
   newcomers — it looks like a permissions/env problem, it's just a missing
   target.

3. **`esp-idf-*` crate versions in `Cargo.toml`**, kept reasonably current and
   in sync with each other (see the `c_char` issue below).

## Common failure modes, in the order they tend to surface

### "Unsupported target '\<host-triple\>'"
`.cargo/config.toml` is missing or doesn't set `target`. Add it (see above).
Do **not** "fix" this by setting `CARGO_BUILD_TARGET` or `RUSTC` env vars —
that's explicitly called out by the esp-idf-svc maintainer as making things
worse, not better.

### "can't find crate for `core`" / "the xtensa-esp32-espidf target may not be installed"
Wrong rustc is active — `rust-toolchain.toml` is missing or not pinning
`channel = "esp"`. Confirm with `rustc --version` (should print a `-nightly`
version tied to the `esp` toolchain) and `rustup toolchain list -v`.

### `expected *const u8, found *const i8` (or similar signedness mismatches) compiling `esp-idf-svc`
Known issue: [esp-rs/esp-idf-sys#375](https://github.com/esp-rs/esp-idf-sys/issues/375).
Older `esp-idf-svc`/`esp-idf-sys` releases assumed a signed `c_char`; newer
Xtensa toolchains generate bindgen output with unsigned `c_char`. Fix by
**upgrading**, not patching: bump `esp-idf-svc`, `esp-idf-hal`, `esp-idf-sys`
to their latest released versions together (check crates.io — `cargo search
esp-idf-svc` etc.). Downgrading rustc to pre-2025 stable is the maintainer's
documented alternative if upgrading isn't an option, but upgrading is simpler
and has no other downsides.

### "cannot find `espidf` in `embuild`" (build.rs failure)
Bumping the `esp-idf-*` crates alone can break the **build-dependency** on
`embuild` in `[build-dependencies]` — if it's pinned to an older minor version
than what `esp-idf-sys` now pulls in, Cargo's feature unification stops
enabling the `espidf` feature on the shared `embuild` instance that
`build.rs`'s `embuild::espidf::sysenv::output()` needs. Fix: bump the
`embuild` build-dependency version to match/track what `esp-idf-sys` now
requires.

### `undefined reference to '__embassy_time_queue_item_from_waker'` (or similar) at link time
The `embassy-time-driver` feature was enabled on `esp-idf-svc` without an
actual embassy executor in the binary to back it. If nothing in the project
uses `embassy`/`async`, just remove the feature — it's not needed for a
blocking/thread-based design. If the project *does* use embassy, a matching
executor providing that symbol must also be present.

### MQTT client: `subscribe()`/`publish()` fails immediately with `ESP_FAIL` right after connecting
`EspMqttClient::new()` returns before the connection handshake completes —
calling client methods immediately after construction is an inherent race,
not a flaky-network symptom. See esp-rs/esp-idf-svc#441. **Do not** gate
`subscribe()` on receiving a `Connected` event from the `connection.next()`
loop — that pattern has been observed to hang indefinitely with no further
events ever delivered (no error, no timeout — just silence). Instead, mirror
the official example pattern:
- One thread does nothing but `while let Ok(event) = connection.next() { ... }`
  — this pumping is what drives the client's internal state machine, and must
  never stop.
- A **separate** thread calls `subscribe()` in a loop, retrying every ~500ms
  on failure, independent of any event. The first attempt is *expected* to
  fail with `ESP_FAIL` before the connection completes — that's normal, not
  a bug to silence.

### MQTT client: messages are received once, then everything goes silent (no crash, no further events)
Classic deadlock: a blocking client call (`publish()`, `enqueue()`, etc.) was
made from inside the same thread that calls `connection.next()`. That thread
must stay free to keep draining events — blocking it inside a client method
call freezes the whole connection after exactly one event, with no error
logged. Fix: route anything the event-handling code needs to publish through
a channel (`std::sync::mpsc`) to a **separate** thread that owns the client
and does the actual blocking call.

## Flashing and monitoring gotchas

- `espflash flash --monitor <path>` builds+flashes+attaches in one step, but
  if run through a non-interactive harness/agent, monitor needs
  `--non-interactive` or it fails with `Failed to initialize input reader`.
- `espflash monitor` (default) does a **hard reset** on attach — expect the
  device to reboot and replay its full boot log every time you attach.
- `espflash monitor --no-reset` is unreliable for capturing live output in
  scripted/non-interactive contexts — it may attach without ever streaming
  further UART data. Prefer the default (reset-based) monitor and budget
  ~6–8s for `espflash`'s own attach overhead (bootloader stub handshake)
  *on top of* the device's own boot-to-WiFi-connect time (~3–5s) before
  expecting to see application-level log lines.
- Target triple in build output paths (`target/<triple>/release/<bin>`)
  depends on the exact `esp-idf-hal`/`espup` version — don't hardcode it in
  scripts; read it from the actual `cargo build` output or `.cargo/config.toml`.

## Local MQTT broker for testing (Mosquitto via Homebrew, macOS)

`mosquitto` 2.x defaults to listening on **loopback only**
(`127.0.0.1`/`::1`) unless a `listener` is explicitly configured — even with
an otherwise fully-default `mosquitto.conf`. A device on the same LAN will
get `select() timeout` / `Error transport connect` trying to reach it. Check
with `lsof -nP -iTCP:1883 -sTCP:LISTEN` — if it shows only loopback
addresses, add to `mosquitto.conf`:
```
listener 1883 0.0.0.0
allow_anonymous true
```
then `brew services restart mosquitto`. Also verify the broker's actual LAN
IP (`ipconfig getifaddr en0` on macOS) matches what the device is configured
to connect to — don't assume a previously-used IP is still correct.
