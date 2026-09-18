# INTENT.md — why this project exists

`README.md` says *what* the firmware does and *how* to build it. `IMPLEMENTATION_PLAN.md` and
`DEVELOPMENT_JOURNEY.md` record *how* specific decisions and bring-up problems were resolved.
This file records the *why*: the purpose, the boundaries, and the invariants that any future
change — by a human or an AI assistant — should preserve or deliberately renegotiate.

## Purpose

Give an AI agent ("Hermes", running on a Mac Mini on the same LAN) a **physical output** it can
act on in the real world, using the simplest possible actuator: an LED on an ESP32.

The LED is not the point. The point is a complete, verified, end-to-end path:

```
agent decision → MQTT publish → WiFi → ESP32 firmware → GPIO → something visibly changes
```

Every part of that path is real — a real broker, a real board, real flash-and-observe
verification. Nothing is simulated, and nothing is stubbed "for now."

## Goals

- **A stable MQTT contract the agent can rely on.** Topics and payload formats are the public
  interface of this repo. The firmware side is here; the agent's wrapper script is deliberately
  elsewhere (see Non-goals).
- **Observable state at all times.** The device publishes status on every command, on every
  rejection, on boot-time defaults, and on a 30s heartbeat. An agent should never have to guess
  what the device is doing.
- **Legible physical feedback without a network round-trip.** Someone standing in the room should
  be able to tell "connected and idle" from "can't reach the broker" by looking at the LED — hence
  the two boot-time defaults (solid on vs. fast blink).
- **Fail loudly, fail early.** Missing WiFi credentials `bail!` at startup instead of retrying
  silently. Invalid commands are rejected *and reported* on the status topic, not dropped.
- **A reusable ESP32-Rust reference.** This was also the vehicle for learning the
  `espup` / `esp-idf-svc` std stack. The transferable lessons live in the global `esp32-rust-idf`
  skill; the project-specific ones stay in `DEVELOPMENT_JOURNEY.md`.

## Non-goals

These are deliberate omissions, not gaps waiting to be filled. Don't "fix" them unprompted.

- **The agent-side wrapper script.** It lives outside this repo. This repo owns only the firmware
  half of the MQTT contract.
- **A general-purpose IoT device framework.** One board, one LED, two commands. No device
  registry, no OTA, no provisioning flow, no plugin architecture.
- **A test suite / lint config / CI.** Verification is flashing to hardware and observing
  behavior. A unit test of `build_delay_table()` would prove almost nothing that the LED doesn't
  prove better.
- **Runtime configuration.** Config is baked in at compile time by `toml_cfg`. The known cost —
  secrets extractable from the flash image — is accepted for a LAN toy, and documented rather
  than papered over. (Using a `.local` mDNS hostname instead of a literal IP for `mqtt_url`
  doesn't count as runtime configuration — the value is still a compile-time-baked string;
  only its DNS resolution happens dynamically, on every connect/reconnect, via ESP-IDF's lwIP.)
- **Security hardening.** No TLS, no broker auth, no signed commands. This runs on a trusted home
  LAN. Anyone who can reach the broker can blink the LED, and that is fine.
- **Small, safe-looking dependencies.** `serde_json` would be the obvious "cleanup" for the
  hand-formatted status payloads. It is not worth the binary size on a flash-constrained target
  for four fields.

## Invariants

Properties that changes must preserve. Each one exists because violating it caused a real problem
or would break the contract the agent depends on.

1. **`switch` never resumes blinking.** `on` and `off` are static states that bypass the blink
   timing loop entirely. `switch toggle` after `blink 5` lands on `Off`, deterministically.
2. **Mode is the top-level state; level only means something in `Blink`.** `level` and `delay_ms`
   are both `null` in status whenever `mode != "blink"` — reporting a stale level would be as
   misleading as reporting a stale delay.
3. **The connection-draining thread never blocks.** No `subscribe`, `publish`, or `enqueue` from
   it, ever. That deadlock was hit for real (`DEVELOPMENT_JOURNEY.md` #9); the status channel and
   the separate subscriber thread exist solely to keep that thread pumping.
4. **Boot defaults are one-time, and gated on the right flag.** The offline fallback checks
   `subscribed`, not `got_real_command`, so a late-connecting broker can't be clobbered back into
   the fast-blink distress signal.
5. **The compile-time delay table stays compile-time.** No runtime float math or heap allocation
   on the microcontroller. The `while` loop in `build_delay_table()` is a `const fn` requirement,
   not a style choice.

## Known trade-offs, consciously accepted

| Trade-off | Why it was accepted | What would change it |
|---|---|---|
| Secrets embedded in the binary | Simplicity; trusted LAN; single device | A second device, or anything leaving the house |
| Linear level→delay mapping | Easy to reason about and verify | Wanting each level to feel like an equal visual step (exponential curve) |
| No retained status / no LWT | Heartbeat covers the gap within 30s | An agent that needs last-known state immediately on subscribe |
| Everything in one `main.rs` | ~One screen per concern; no navigation cost | A third command, or any non-LED peripheral |

## When this file is wrong

If a change makes one of the invariants above false, the invariant is not automatically the
loser — but the change should say so out loud and update this file in the same commit. A silent
violation is the failure mode this document exists to prevent.
