# spec.md — Requirements & design spec: integrating `INTENT.md` into the codebase

**Input:** `INTENT.md` (this repo, committed `76991c8`)
**Status:** Draft for engineering review
**Scope:** How the codebase should embody, enforce, and stay in sync with the goals, non-goals,
and invariants `INTENT.md` documents — not new product functionality.

## 0. Framing — read this before the rest

`INTENT.md` is not a feature request. It's a rationale document for behavior that is **already
implemented and already flashed to hardware** (see `IMPLEMENTATION_PLAN.md`'s 2026-08-11 test
log). "Integrating it into the codebase" therefore means something narrower than it would for a
new feature: making sure the *code* actually satisfies every claim the *doc* makes, and putting
just enough process around that pairing that they don't drift apart. This spec is written on that
basis. Where a section below would normally cite a company brand guide, security policy, or UX
standard, see §5 for why none apply here and what was used instead.

## 1. Background

- The firmware (`src/main.rs`, single file, ~390 lines) already implements the `mode`/`switch`
  state machine, the two boot-time defaults, the compile-time delay table, and the hand-formatted
  JSON status payloads that `INTENT.md` describes.
- `INTENT.md` was added to give future changes (human or AI-assisted) a single place to check
  "is this deliberate?" before altering behavior that looks like a bug but isn't.
- This spec's job is to turn `INTENT.md`'s five invariants and its non-goals list into concrete,
  checkable engineering requirements, and to flag where the current codebase is weaker than the
  doc implies.

## 2. Requirements

### 2.1 Functional requirements (already implemented — verify, don't rebuild)

| ID | Requirement | Source (INTENT.md) | Current implementation |
|---|---|---|---|
| FR-1 | `switch on`/`off` bypass the blink timing loop entirely; `switch toggle` after any `blink` command deterministically resolves to `Off`. | Invariant 1 | `src/main.rs:199-212` (switch handler), `:326-351` (main loop `match mode`) |
| FR-2 | `level`/`delay_ms` in status payloads are `null` whenever `mode != "blink"`. | Invariant 2 | `src/main.rs:359-380` (`publish_status`) |
| FR-3 | The connection-draining thread performs no blocking client call (`subscribe`/`publish`/`enqueue`). | Invariant 3 | `src/main.rs:167-234` (drain loop) — status changes go out over `status_tx`, an `mpsc::Sender`, never a direct client call |
| FR-4 | The offline-fallback boot default is gated on `subscribed`, not `got_real_command`. | Invariant 4 | `src/main.rs:298-304` (offline-fallback thread body) checks `subscribed`; `got_real_command` is a separate flag set only by real commands (`:179`, `:184`, `:212`) |
| FR-5 | The level→delay mapping is computed at compile time with no heap allocation. | Invariant 5 | `src/main.rs:62-84` (`const fn build_delay_table`, `while`-loop) |

**Requirement for this integration work specifically:** none of FR-1..FR-5 should change as a
*side effect* of this spec. This spec's deliverable is verification + process, not new behavior.

### 2.2 Non-functional / process requirements (new — this is the actual scope of "integration")

| ID | Requirement |
|---|---|
| NFR-1 | Every future PR/commit that touches `mode`, `switch`, boot-time defaults, `cfg.toml`/`toml_cfg` config loading, or `build_delay_table()` must state in its commit message or PR description whether it preserves or knowingly changes an `INTENT.md` invariant. |
| NFR-2 | If a change knowingly changes an invariant, `INTENT.md` must be updated in the **same commit** (this is `INTENT.md`'s own closing rule — see "When this file is wrong" — this spec just makes it a checkable requirement rather than a suggestion). |
| NFR-3 | `CLAUDE.md` already points AI assistants at `INTENT.md` before state-machine/boot-default/config changes (commit `76991c8`), and `AGENTS.md` was created with the same pointer already in place (commit `b177ed0`). Human-facing equivalent: add the same pointer to any future `CONTRIBUTING.md`, should one be created. |
| NFR-4 | The traceability table in §2.1 should be re-verified (line numbers, at minimum) whenever `src/main.rs` is restructured, since it's the only artifact tying the doc's claims to actual code locations. |

### 2.3 Explicit non-requirements (carried over from `INTENT.md` §Non-goals)

Do not, as part of this integration work: add a test suite or CI, add runtime configuration, add
TLS/broker auth, add `serde_json`, split `main.rs` into modules, or build the agent-side wrapper
script. `INTENT.md` names all of these as deliberate omissions. Reopening any of them is a
separate decision the user makes explicitly, not a byproduct of this spec.

## 3. Design

### 3.1 What "integration" actually looks like here

There is no new module, no new dependency, and no new runtime behavior. The design is:

1. **Traceability, not enforcement.** Given the no-test/no-CI non-goal (`INTENT.md` §Non-goals,
   "A test suite / lint config / CI"), invariants FR-1..FR-5 are **not** proposed to get automated
   checks. The existing verification method — flash to hardware, observe LED + MQTT status,
   record in `IMPLEMENTATION_PLAN.md`'s test log — stays authoritative. This spec's §2.1 table is
   the traceability layer: a reviewer (human or AI) can jump from an invariant to the exact lines
   that implement it without re-deriving it from scratch.
2. **A single source of truth for "why", cross-referenced, not duplicated.** `INTENT.md` already
   sits alongside `README.md` (what/how), `CLAUDE.md`/`AGENTS.md` (architecture for assistants),
   and `IMPLEMENTATION_PLAN.md`/`DEVELOPMENT_JOURNEY.md` (decision/bug history). No content moves;
   this spec adds no new doc file beyond itself, and itself is a one-time artifact for this
   integration pass rather than a doc meant to be kept current like the others (see §6).
3. **Process lives in commit discipline, not tooling.** NFR-1/NFR-2 are enforced by review
   habit (this is a single-maintainer repo — see prior session's recorded preference for direct,
   small, explicit commits), not a CI gate, consistent with the no-CI non-goal.

### 3.2 Sequence: how a future invariant-touching change should flow

```
1. Change proposed (e.g. "make switch toggle resume last blink level")
2. Check INTENT.md's invariants + non-goals list
3a. Doesn't conflict → implement, verify on hardware, done.
3b. Conflicts (e.g. this one conflicts with Invariant 1) →
    - Surface the conflict explicitly to the user/reviewer (don't silently override)
    - If accepted: update INTENT.md's invariant text + this spec's §2.1 table in the same commit
    - If rejected: the request itself changes (e.g. propose it as a *new*, named mode instead)
```

This is already how `CLAUDE.md`/`AGENTS.md` instruct AI assistants to behave (see the pointer
added in commit `76991c8`); §3.2 just makes the flow explicit for the spec's own sake.

## 4. Traceability matrix: goals → evidence

| INTENT.md goal | Where it's evidenced today |
|---|---|
| Stable MQTT contract | `README.md` "Usage" section (topics/payloads), unchanged since `321305b` |
| Observable state at all times | `publish_status()` is called directly from 2 sites (status-publisher thread draining the channel, `:243`; heartbeat thread, `:317`). That channel is itself fed by 3 distinct senders: the command handler in the drain thread, the subscriber thread's connect-time default (`:283`), and the offline-fallback thread (`:303`). |
| Legible physical feedback without network round-trip | Boot defaults: solid on (`MODE_ON`, `:281-283`) vs. fast blink (`MODE_BLINK`/`MAX_LEVEL`, `:301-303`) — see §5.2 for a UX note |
| Fail loudly, fail early | `anyhow::bail!` on empty `wifi_ssid` (`:93`); invalid `switch` payload rejected + reported (`reason:"rejected_invalid_switch"`) |
| Reusable ESP32-Rust reference | `esp32-rust-idf` global skill + `DEVELOPMENT_JOURNEY.md` |

## 5. Brand, security, and UX standards — applicability

The request asked for this spec to conform to brand guidelines, security policies, and UX
standards. None of those exist as documents for this project, and per your direction this section
records that gap rather than inventing standards to fill it.

### 5.1 Brand guidelines: not applicable

This is single-binary embedded firmware with no visual surface (no app, no web UI, no printed
material). There is nothing for a brand guideline to attach to. No action taken.

### 5.2 UX standards: no formal standard, but the LED *is* a UX surface — noted informally

`INTENT.md` itself states a UX-shaped goal: "someone standing in the room should be able to tell
'connected and idle' from 'can't reach the broker' by looking at the LED." That's a legibility
requirement even without a formal UX doc. Current design already satisfies it (two visually
distinct signals: solid vs. fast blink), but two gaps are worth naming for the engineering team,
not fixed unprompted:

- **Only two signals exist.** A rejected command, a mid-session disconnect, and normal `Blink`
  mode at a slow level can all *look* similar to a human observer with no MQTT tooling in hand.
  This is consistent with the non-goal of "no device registry / no elaborate signaling," so it's
  a note, not a defect.
- **No accessibility consideration for the LED signal itself** (e.g. colorblind-safe — moot here
  since it's a single-color LED and only timing, not color, encodes state — but flagged since a
  future board revision with an RGB LED would reopen this).

### 5.3 Security policies: not applicable by explicit design, residual risk documented

`INTENT.md`'s non-goals explicitly rule out TLS, broker auth, and signed commands, accepting that
"anyone who can reach the broker can blink the LED, and that is fine" for a trusted home LAN. This
spec does not propose adding security controls — that would directly contradict a documented
non-goal and is not this integration's job. For completeness, the residual risks already named in
`INTENT.md`/`README.md` are restated here so the engineering team sees them in one place:

- Secrets (`wifi_ssid`, `wifi_pass`, `mqtt_url`) are compiled into the flash image and extractable
  with physical/flash access. Accepted trade-off, not a defect.
- No MQTT auth/TLS — anyone on the LAN (or anyone who can reach the broker port) can publish
  commands or read status. Accepted trade-off, not a defect.
- No LWT / no retained status — a subscriber can't distinguish "device is off" from "device is on
  but hasn't published in the current heartbeat window" for up to 30s. Documented gap, not
  addressed here.

If any of these change (e.g. a second device joins the network, or the device leaves the trusted
LAN), that's a scope change to `INTENT.md` itself, not something this spec should decide.

### 5.4 Global skills reviewed for applicability

Checked the full global skill roster (`~/.claude/skills/`) rather than assuming only the
project-scoped ones apply:

| Skill | Applies here? |
|---|---|
| `esp32-rust-idf` | Yes — already the referenced toolchain/MQTT-runtime playbook for this repo (`README.md`, `CLAUDE.md`). No new guidance needed for this spec. |
| `local-mqtt-broker` | Yes — network/host-side MQTT diagnosis, already referenced from `IMPLEMENTATION_PLAN.md`. Not exercised by this spec since no connectivity work is proposed. |
| `rust-best-practices` | Yes, generally — but `INTENT.md`'s non-goals (no test suite, single-file `main.rs`, no premature module split) explicitly override the general module-organization/testing guidance for *this* project. Followed where it doesn't conflict with a stated non-goal. |
| `software-engineering-best-practices` | Yes — its "write a design doc before a nontrivial architecture change" guidance is exactly what `INTENT.md` and this spec are doing. No conflict. |
| `htmx4`, `keycloak-admin`, `list-pagination-bulk-actions`, `tailwindcss4`, `temporal-admin`, `frontend-design`, `dataviz` | No — all target web/UI/auth-service stacks. This repo has no web surface, no login flow, and no data visualization, so none apply. This is also why §5.1/5.2 found no brand or UX *skill* to invoke, not just no project-level doc. |

## 6. Areas of concern

Ranked by what's most likely to actually bite:

1. **This spec's traceability table (§2.1, §4) will silently go stale.** Line numbers are a
   snapshot; the first refactor of `src/main.rs` invalidates them with no mechanism to notice.
   Given the no-CI non-goal, there's no automated way to catch this. Recommend treating this
   spec as a one-time integration artifact (useful now, not maintained going forward) rather than
   a living doc — `INTENT.md` is the living doc; this file is not.
2. **"Hand to the engineering team" doesn't quite fit a single-maintainer repo.** There is no
   second reviewer to enforce NFR-1/NFR-2 in practice; they rely on the same person (or their AI
   assistant) remembering to apply them. If a second contributor joins, this spec's process
   section (§3.3) should graduate into an actual `CONTRIBUTING.md` with the check made explicit
   in PR templates.
3. **The request's framing (brand/security/UX skills, "engineering team") suggests this prompt
   may have been written for a different, product-shaped codebase** and pointed at this repo by
   mistake, or reused as a template. Worth double-checking that a product-facing spec wasn't
   actually intended for another project — nothing in this repo's history suggests those
   standards were ever meant to apply here.
4. **No mechanism enforces "update INTENT.md in the same commit" (NFR-2) beyond habit.** This
   mirrors the intentional lack of CI elsewhere in the project, so it's consistent, but it does
   mean the guarantee is only as strong as reviewer discipline.

## 7. Open questions for the user

- Should this spec (`spec.md`) be committed to the repo, or was it meant as a one-off deliverable
  outside version control? Given §6.1, recommend **not** committing it as a permanent doc (it
  would join `README.md`/`CLAUDE.md`/`AGENTS.md`/`IMPLEMENTATION_PLAN.md`/`DEVELOPMENT_JOURNEY.md`/
  `INTENT.md` as a seventh root-level doc, most of which would duplicate `INTENT.md` within a few
  commits).
- Confirm §6.3's concern is unfounded — i.e. this repo is genuinely the intended target for a
  "brand guidelines / security policies / UX standards / engineering team" framing.
