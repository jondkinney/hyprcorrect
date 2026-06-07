# Plan: rubber-band kinetic scroll for the macOS prefs window

Status: **IMPLEMENTED 2026-06-06** (feel validated on-device; release wiring
pending — see below). Owner context: hyprcorrect prefs (eframe/egui), kanso
design system, the cohort egui-winit/winit forks.

## What actually shipped (leaner than the plan below)

The plan's central unknown — "does winit expose `momentumPhase`?" — resolved
in our favour: **winit's macOS backend already reads `momentumPhase()` AND
`phase()`**, it just *collapses* them into one `TouchPhase`. So no new NSEvent
reading was needed — only un-collapsing. Implemented as (branches
`macos-scroll-phase` in each fork; `cohort-fork` feature + `prefs.rs` in
hyprcorrect):

- **winit-cohort**: new `ScrollMomentumPhase {Gesture,GestureEnded,Momentum,
  MomentumEnded}` + `WindowEvent::ScrollMomentumPhase`, classified un-collapsed
  in `scrollWheel:` and emitted alongside the unchanged `MouseWheel`.
- **egui-winit-cohort**: maps it to a `u8` (`1..=4`) published to egui memory at
  `SCROLL_PHASE_ID = "egui_scroll_phase"` (mirrors the hold-gesture `bool`).
- **kanso** `scroll_view`: drops Momentum deltas (guarded `!wheel`), flings off
  the precise `GestureEnded` lift, gated on the slot being `Some` so Linux is
  byte-identical. Plus **rest-to-stop**: a zero-delta `Gesture` phase
  (MayBegin/Began) halts the coast — macOS has no Wayland-style `HoldGesture`.
- **hyprcorrect**: `cohort-fork` cargo feature switches macOS to `scroll_view`
  (else native `ScrollArea`); `macos_scroll_tuning()` overrides only
  `rb_amplitude` 3.0 → **0.5** (macOS carries far more edge velocity than
  libinput). Tuned live with the kanso gallery sliders.

Captured as skills: `macos-egui-winit-collapses-scroll-momentum-phase` and
`macos-trackpad-rest-to-stop-is-zero-delta-maybegin`.

**Still pending (gated on push authorization):** the release sequence — push the
fork branches, cut kanso 0.1.5, bump pins + add the `[patch.crates-io]` step and
`--features cohort-fork` to the `macos-aarch64` job in `release.yml`. Until then
the prebuilt DMG stays on the native ScrollArea (no regression, no bounce).

---

_Original plan (superseded by the leaner implementation above), kept for the
physics/tuning notes:_

## Goal
Give the macOS Preferences window the full **kinetic fling + rubber-band
over-scroll** that Linux already has via `kanso::scroll::scroll_view` —
the elastic bounce at the top/bottom edges, matching Finder/Safari — while
keeping the native momentum feel we already get on macOS.

## Where we are now (end of the previous session)
- **Linux/Wayland:** `kanso::scroll::scroll_view` owns the scroll offset and
  synthesizes the whole motion (drag → fling → rubber-band bounce) from raw
  scroll deltas. libinput sends *no* OS momentum, so kanso must invent it.
- **macOS (current):** the prefs content renders in a **plain
  `egui::ScrollArea`** (see `crates/hyprcorrect-ui/src/prefs.rs`, the
  `#[cfg(target_os = "macos")]` branch in `update`). egui applies the OS's
  native momentum deltas directly → smooth kinetic coast, **but egui clamps
  at the edges, so there is no rubber-band bounce.**
- `kanso::scroll::scroll_momentum(ctx)` (the ctx-level momentum injector for
  *plain* scroll areas) is gated `#[cfg(not(target_os = "macos"))]`.

## Why `kanso::scroll::scroll_view` pinned on macOS (the thing to fix)
`scroll_view` infers the **finger lift** from a *gap in scroll events*
(`LIFT_GAP` / `COAST_GAP` in `kanso/src/scroll.rs`): "no events for ~45 ms ⇒
fingers lifted ⇒ start the fling from the tracked velocity." That works on
libinput, which stops sending events the instant you lift.

macOS does **not** stop: after the physical lift the OS keeps streaming
*momentum-phase* scroll events as the coast decays. So `scroll_view` never
sees the gap, stays in `Dragging` through the entire momentum tail, and by
the time the events finally stop the tracked velocity has already decayed to
~0 → no fling fires → it "pins" at the release point. The Apple trackpad
genuinely sends something different (OS-synthesized momentum) than the
Linux path expects.

**Implication:** the fix hinges on knowing the macOS scroll *phase*
(gesture vs. momentum vs. ended), which egui flattens away today.

## Recommended approach — make `scroll_view` phase-aware on macOS (reuse the physics)
Reuse all of kanso's tuned fling + rubber-band; only fix the lift detection
and suppress the doubled OS momentum.

### 1. Surface the macOS scroll phase (the enabling piece)
winit's `WindowEvent::MouseWheel` carries `phase: TouchPhase`, but egui's
`Event::MouseWheel` drops it. The cohort already forks egui-winit
(`egui-winit-cohort`, pinned by rev in `.github/workflows/release.yml`'s
`[patch.crates-io]`) and publishes the Wayland hold gesture into egui memory
at `Id::new("egui_hold_gesture_active")` (read by `hold_gesture_active` in
`kanso/src/scroll.rs`). **Mirror that mechanism for the scroll phase.**

- In the fork's macOS backend, classify each scroll NSEvent using
  `NSEvent.phase` **and** `NSEvent.momentumPhase`, and publish a small enum
  into egui memory each frame, e.g. `Id::new("egui_scroll_phase")`:
  `Gesture` (finger down + moving), `Ended` (finger lifted),
  `Momentum` (OS coast), `MomentumEnded` / `Idle`.
- **Verify first:** does `winit` (and our `winit-cohort` pin) already expose
  `momentumPhase`, or does it collapse momentum into `TouchPhase::Moved`?
  If collapsed, the fork must read `NSEvent.momentumPhase()` directly in the
  macOS event handler. This is the main unknown — confirm before estimating.
- This is additive and behind the existing fork; non-macOS is unaffected
  (slot simply never written, like the hold gesture).

### 2. Teach `kanso::scroll::scroll_view` to use the phase on macOS
In `kanso/src/scroll.rs` (kanso is the user's own published crate — needs a
kanso release afterward):
- When the `egui_scroll_phase` slot is present (macOS), drive the state
  machine from it instead of the event-gap inference:
  - `Gesture` deltas → apply + `track_velocity` (as today while `Dragging`).
  - `Ended` → the precise lift: fire the fling from `vel_ema` (the existing
    `Flinging` / `begin_bounce` path). This is exactly what `LIFT_GAP` was
    approximating, now exact.
  - `Momentum` deltas → **drop them** (do not apply): kanso's synthesized
    fling *replaces* the OS momentum. Applying both stacks velocity.
- The existing `Flinging → into_edge → begin_bounce → rb_elastic` path then
  produces the rubber-band — no new physics, it just finally gets a real
  fling to carry velocity into the edge.
- Re-touch mid-coast: a fresh `Gesture` phase stops the fling (kanso already
  halts on a raw delta during `Flinging`; with the phase it's cleaner — the
  macOS "touch to stop").
- Keep the gap-inference path untouched for non-macOS (no phase slot ⇒
  identical to today). Gate the new path on the slot's presence, not a `cfg`,
  so kanso stays platform-agnostic.

### 3. Add a macOS `ScrollTuning` profile
`MomentumConfig` / `ScrollTuning` defaults were measured against **libinput**
deltas (release velocities ~300–1800 px/s; `FLING_FRICTION`, `VEL_TAU`,
`RB_*`, etc.). macOS pixel-delta magnitude and event cadence differ, so the
fling distance and bounce will feel off with the Linux numbers. Plan a
tuning pass:
- Re-measure macOS trackpad release velocities (log `vel_ema` at `Ended`).
- Use `kanso::scroll::set_scroll_tuning(ctx, macos_profile)` (already exists,
  refreshed per frame) to apply a macOS profile.
- **Tune against a release build** (`cargo build --release`) — `VEL_TAU` etc.
  are frame-rate-independent but the feel is calibrated for release. The
  kanso gallery (`kanso/examples/gallery.rs`) has live tuning sliders; run it
  under macOS to dial it in.

### 4. Switch the macOS prefs back to `scroll_view`
In `crates/hyprcorrect-ui/src/prefs.rs`, remove the `#[cfg(target_os =
"macos")]` `egui::ScrollArea` branch added this session and use
`kanso::scroll::scroll_view` for all platforms again (it's now phase-aware).
The section-change reset (`scroll_view_reset`) already works everywhere.
Re-check whether `scroll_momentum(ctx)` should stay `cfg(not macos)` — prefs
uses `scroll_view` (owned offset), which doesn't rely on `scroll_momentum`,
so it can stay Linux-only; revisit only if a plain `ScrollArea` elsewhere
needs momentum on macOS.

## Alternatives considered (and why not)
- **B — keep `egui::ScrollArea`, add an overscroll-bounce layer driven by OS
  momentum.** To over-scroll you must own the offset (egui clamps), so you'd
  end up re-implementing `scroll_view`'s owned-offset render path anyway,
  minus the reuse. Only attractive if the egui-winit fork work proves
  infeasible.
- **Heuristic momentum detection (no fork)** — guess gesture-vs-momentum from
  the delta decay signature. Fragile and version-dependent; rejected.

## Risks / unknowns
- **winit momentum phase exposure** (the big one) — see step 1. May require a
  `winit-cohort` change in addition to `egui-winit-cohort`. Both are pinned
  by rev in `release.yml`; new revs + bumped pins needed.
- **Tuning** — expect a real pass; the Linux numbers won't transfer 1:1.
- **Double momentum** — must drop `Momentum`-phase deltas; verify no
  stacking.
- **Fork maintenance** — adds another macOS-specific behavior to the cohort
  fork; document it next to the hold-gesture code.

## Test checklist
- Two-finger flick → coast **and** bounce at the edge (Finder/Safari feel).
- Re-touch mid-coast → stops dead (touch-to-stop).
- Slow drag past the edge → rubber-band stretch + spring-back.
- Mouse wheel (not trackpad) → plain stepped scroll, no bounce
  (`scroll_is_wheel` path).
- **Linux regression:** unchanged — no phase slot ⇒ gap inference as today.
- Section switch → opens at top (reset still works).

## Files in play
- `egui-winit-cohort` (fork) — publish `egui_scroll_phase` on macOS;
  possibly `winit-cohort` for `momentumPhase`.
- `kanso/src/scroll.rs` — phase-aware `scroll_view`; macOS `ScrollTuning`
  profile. (kanso is published to crates.io → cut a release after.)
- `crates/hyprcorrect-ui/src/prefs.rs` — macOS back to `scroll_view`.
- `.github/workflows/release.yml` — bump the `[patch.crates-io]` fork pins.
