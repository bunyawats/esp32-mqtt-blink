# Implementation plan: `switch` command + AI-agent control

Status: **implemented, flashed, and verified end-to-end against real
hardware and the local Mosquitto broker** (2026-08-11). Only the doc-update
checklist at the bottom remains. This file exists so progress can be tracked
and resumed across separate Claude Code sessions — check the box states
below before starting work, and update them as you go.

## Context

An AI agent ("Hermes") running on a Mac Mini on the same LAN will control this
device by shelling out to `mosquitto_pub`/`mosquitto_sub` (or similar) against
the existing local Mosquitto broker (see the `local-mqtt-broker` skill for
broker reachability diagnosis). The wrapper script Hermes uses is being built
separately, outside this repo/session — this plan only defines the MQTT
contract (topics + payload formats) and the firmware-side state machine it
needs to implement. **No wrapper script is part of this plan's scope.**

## Design decisions

Confirmed with the user (2026-08-11):

1. The LED has **three mutually exclusive top-level modes** — not a level
   plus a separate on/off gate, and not "off" as a side-effect of level 0:
   - **`Off`** — LED held low, blink loop completely skipped.
   - **`On`** — LED held high, blink loop completely skipped.
   - **`Blink`** — exactly today's existing behavior: `level_to_delay_ms`
     drives a timed on/off loop using `blink_level` (0 still renders as
     "off"-looking inside this mode, unchanged from today, but that's
     `Blink` mode producing a degenerate always-off pattern — it is not the
     same code path as top-level `Off` mode).
   `Off` and `On` are symmetric: both are static states where the main loop
   just holds the pin and polls for a new command, with zero blink-timing
   logic executing. Only `Blink` mode ever touches `level_to_delay_ms`.
2. Command → mode mapping:
   - `blink <0-10>` (existing topic/payload, unchanged) → sets
     `blink_level`, **and sets `mode=Blink`**.
   - `switch on` → `mode=On`.
   - `switch off` → `mode=Off`.
   - `switch toggle` → if current mode is `Blink` or `On` (i.e. "currently
     on" in either form) → `mode=Off`. If current mode is `Off` →
     `mode=On` (toggle's "on" outcome is the static `On` mode, not a resume
     of whatever `blink_level` used to be — toggle belongs to the switch
     command, it doesn't reach into blink state).
   - This was an explicit requirement: `blink 5` then `switch toggle` must
     deterministically land on `Off`, which it does since `Blink` counts as
     "on" for the toggle's before-state.
3. Boot-time defaults, composing the two original asks and giving two
   visually distinct, intuitive signals:
   - Track `got_real_command: bool`, set `true` the first time any real
     `blink` or `switch` command (including a rejected/invalid one — it's
     still real inbound traffic) is processed from the network.
   - Track a **second, separate** flag `subscribed: bool`, set `true` once
     both `subscribe()` calls succeed (before any "connected" default is
     applied).
   - If MQTT hasn't subscribed within a boot window (~10s after WiFi
     connects) — checked against **`subscribed`**, not `got_real_command` —
     apply `mode=Blink, blink_level=10`: fast-blink offline/no-broker
     signal.
   - Once subscribe succeeds, if `got_real_command` is still `false` at that
     point: apply `mode=On` — solid light = "alive, connected, no command
     yet". Does not touch `blink_level`.
   - Once `got_real_command` is `true`, the connect-time default no longer
     fires — a real command always wins.
   - **Why two separate flags, not one:** gating the offline fallback on
     `got_real_command` alone has a race — neither boot default sets
     `got_real_command` (only a real inbound command does), so a subscribe
     that succeeds at, say, t=9s would apply `mode=On`, and the offline
     fallback thread waking at t=10s would see `got_real_command` still
     `false` and incorrectly clobber it back to `Blink`/fast-blink even
     though the device is genuinely connected. Gating the fallback on
     `subscribed` instead closes that race — once subscribed, the fallback
     can never fire, full stop, regardless of `got_real_command`. Applying
     the connect-time default *after* an already-fired offline fallback
     remains intentionally fine (see below) — that's a one-directional
     "was offline, now connected" transition, not the same race.

Assumed (sane defaults chosen without re-confirming — flag if wrong):

4. **Offline-window is a one-time boot-time check**, not a continuous
   re-check on a later mid-session disconnect (e.g. broker restarting after
   hours of uptime does *not* re-trigger the blink=10 fallback).
5. **No persistence across power cycles.** `mode`/`blink_level` reset to the
   boot rules above on every boot, matching the existing stateless RAM-only
   design (`Arc<AtomicU32>`, no NVS involvement for this state).

## MQTT contract

Topic prefix is the bare device namespace `esp32/` — not `esp32/blink/` — since
the device now has more than one "blink"-scoped concern (`switch` isn't
blink-specific). This is also a rename of the two already-implemented
topics, not just new additions:

| Topic (cfg key) | Direction | Payload | Notes |
|---|---|---|---|
| `topic_speed` (existing, **renamed** `esp32/blink/speed_level` → `esp32/speed_level`) | device ← agent | ASCII decimal `0`–`10` | Unchanged parsing/validation. Sets `blink_level`, `mode=Blink`. |
| `topic_switch` (**new**, `esp32/switch`) | device ← agent | `on` \| `off` \| `toggle` (exact lowercase match) | Sets `mode` per the rules above. Non-matching payload → rejected, logged, status published with `reason:"rejected_invalid_switch"`, same pattern as today's out-of-range-level rejection. |
| `topic_status` (existing, **renamed** `esp32/blink/status` → `esp32/status`) | device → agent | JSON | Gains a `"mode"` field: `{"mode":"off"\|"on"\|"blink","level":N|null,"delay_ms":N|null,"reason":"..."}`. Both `level` and `delay_ms` are `null` whenever `mode != "blink"` — neither one describes anything real while the LED isn't in `Blink` mode, so leaving a stale `level` behind would be misleading. New `reason` values: `"switch_on"`, `"switch_off"`, `"switch_toggle"`, `"offline_fallback"`, `"connected_default"`, alongside existing `"updated"`, `"rejected_out_of_range"`, `"heartbeat"`. |

`topic_speed`/`topic_status` renames already applied directly to
`src/main.rs`'s `Config` defaults and to `cfg.toml`/`cfg.toml.example`
(2026-08-11) — the source is updated, but this is compile-time baked config
(see `CLAUDE.md`), so **the physical device is still running the old
`esp32/blink/*` topics until the next `cargo build --release` + reflash.**
`topic_switch = "esp32/switch"` is the new key, still pending implementation
per the checklist below.

## Firmware implementation checklist

All items below are implemented in `src/main.rs` as of 2026-08-11 — **not yet
flashed or tested against real hardware** (see Testing plan below).

- [x] Add `topic_switch` to the `Config` struct (`#[toml_cfg::toml_config]`)
      with default `"esp32/switch"`; add to `cfg.toml.example` and this
      repo's real `cfg.toml`.
- [x] Define mode constants (`MODE_OFF`/`MODE_ON`/`MODE_BLINK`, plain `u8`s —
      not a real Rust `enum`, since `std::sync::atomic` has no generic
      atomic-enum type) and store as `Arc<AtomicU8>` shared state.
- [x] Add `got_real_command: Arc<AtomicBool>` shared state.
- [x] Add `subscribed: Arc<AtomicBool>` shared state (this is the
      race-avoidance flag described in design decision 3 above — not in the
      original plan draft, added during implementation).
- [x] Subscribe to **both** `topic_speed` and `topic_switch` from the
      subscriber-retry thread — implemented as one thread looping over
      `[topic_speed, topic_switch]` sequentially, each with its own
      retry-until-success inner loop, rather than two separate threads.
- [x] Connection-draining thread's event handler now destructures `topic`
      (`Option<&str>`) alongside `data` from `EventPayload::Received` and
      routes on `topic == Some(topic_speed)` vs. `topic == Some(topic_switch)`
      vs. an `else` branch that logs unexpected topics.
- [x] Blink-handler path: same parse/clamp logic as today, plus
      `mode.store(MODE_BLINK, ...)` and `got_real_command.store(true, ...)`
      — including on the out-of-range-rejection branch.
- [x] Switch-handler path: exact match on `"on"`/`"off"`/`"toggle"`; anything
      else is rejected (`reason:"rejected_invalid_switch"`, mode unchanged).
      `got_real_command.store(true, ...)` is set in all cases, including
      rejection, since it's still real inbound traffic.
- [x] Offline-fallback thread: sleeps `OFFLINE_FALLBACK_SECS` (10s) after
      spawn (which happens after `wifi.connect()` returns), then checks
      **`!subscribed.load()`** (not `got_real_command` — see design decision
      3's race writeup) before applying `level=10, mode=Blink,
      reason:"offline_fallback"`.
- [x] Connect-time default: inside the subscriber thread, right after both
      `subscribe()` calls succeed and `subscribed.store(true, ...)` runs, if
      `!got_real_command.load()` applies `mode=On,
      reason:"connected_default"`.
- [x] Main loop: matches on `mode.load()` — `MODE_OFF`/`MODE_ON` are
      symmetric static branches (`led.set_low()`/`set_high()` once, then
      `OFF_POLL_MS` poll, no blink timing at all); the `_` (i.e. `MODE_BLINK`)
      arm is exactly today's `level_to_delay_ms(level)` match.
- [x] `publish_status()`: takes `mode: u8` alongside `level`; both
      `level`/`delay_ms` render as JSON `null` unless `mode == MODE_BLINK`;
      threaded through every call site (draining thread's status_tx sends,
      heartbeat thread, offline fallback, connected default).
- [x] Updated the doc comments on `level_to_delay_ms` and added new ones on
      the `MODE_*` consts and `publish_status()` explaining the mode
      semantics.

Build verified clean (`cargo build --release`, no warnings) after
implementation; hardware flash + the manual testing plan below are still
outstanding.

## Testing plan (manual, against real hardware + local Mosquitto)

All scenarios below were run against real hardware on 2026-08-11 and passed.

- [x] Fresh boot with broker already up → device subscribed to both topics
      within ~5s (well inside the 10s window), applied `connected_default`.
      Verified via boot log (`Subscribed to esp32/speed_level` /
      `Subscribed to esp32/switch` both at ~4915ms uptime).
- [x] Reset with broker down (`brew services stop mosquitto`) → offline
      fallback fired at exactly connect-time + 10s (`WiFi connected` at
      4395ms, `applying offline fallback` at 14405ms), no crash/hang, kept
      retrying `subscribe()` throughout.
- [x] Bring broker up after the offline fallback already fired → device
      subscribed successfully once reachable (19915ms uptime) and applied
      `connected_default` — confirmed live via the next heartbeat:
      `{"mode":"on","level":null,"delay_ms":null,"reason":"heartbeat"}`
      (i.e. the fallback's `mode=Blink` got correctly superseded, and
      `blink_level` staying at 10 internally is informational-only, invisible
      once `mode != Blink` as designed).
- [x] `switch off` → `{"mode":"off","level":null,"delay_ms":null,"reason":"switch_off"}`.
- [x] `switch on` → `{"mode":"on","level":null,"delay_ms":null,"reason":"switch_on"}`.
- [x] `blink 5` → `{"mode":"blink","level":5,"delay_ms":578,"reason":"updated"}`.
- [x] `switch toggle` right after the `blink 5` above → landed on `off`
      (`{"mode":"off",...,"reason":"switch_toggle"}`), confirming the
      explicit toggle-after-blink requirement.
- [x] `switch toggle` from `Off` → landed on `on` (not back into level-5
      blinking).
- [x] `switch toggle` again from `On` → landed back on `off`, confirming a
      full `Off → On → Off` cycle with no intervening blink.
- [x] Invalid switch payload (`flip`) → rejected:
      `{"mode":"off","level":null,"delay_ms":null,"reason":"rejected_invalid_switch"}`,
      mode correctly unchanged from the prior `off`.
- [x] `mosquitto_sub` on `topic_status` confirmed the `"mode"` field appears
      on every reason including `heartbeat`, with `level`/`delay_ms`
      correctly `null` outside `Blink` mode throughout.

## Docs to update once implemented

- [x] `CLAUDE.md` — replaced the "Planned" section with a real architecture
      description (`What this is` + `Architecture` sections rewritten
      2026-08-11) covering the `mode` state model, the six-thread runtime
      shape, and the new status payload shape.
- [x] `README.md` — moved the `switch` usage examples out of "planned"
      framing into the real `## Usage` section (with a new "Boot-time
      defaults" subsection), added `topic_switch` to the `cfg.toml` example
      block, updated the project-layout description, and removed the
      now-redundant "Roadmap" section.
- [x] This file is kept (not deleted) as the permanent design-rationale +
      test-log record for the `switch` command, referenced from both
      `CLAUDE.md` and `README.md`.

Status: **done**. All checklist items above and in the Firmware
implementation / Testing plan sections are complete.
