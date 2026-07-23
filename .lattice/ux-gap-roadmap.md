---
doc: ux-gap-roadmap
created: 2026-07-22
author: Nik
status: living
kind: product-roadmap
note: >
  Cross-cutting UX/feature gap analysis vs competitors — NOT a feature blueprint.
  Feeds design-blueprint per picked feature. Next up: level-meters (see Priority 1).
---

# Splitstream — UX & Feature Gap Roadmap

Competitive deep-dive done 2026-07-22. Engine (P0–P5) is strong and, in guts
(drift recovery, RT-safe convolution, per-process capture), ahead of most
rivals. The gap is almost entirely **UI/UX visibility + a few consumer
features**. Users won't feel the engine quality because they can't *see* it.

## Current surface (baseline)

Groups→outputs routing, per-process WASAPI loopback + session-mute, per-group
gain + global master + follow-master, per-group DSP (parametric EQ one-band-per-
stage, limiter), per-output headroom limiter, cross-group ducking sidechain,
HRTF binaural virtual surround, N→2 channel mixdown, device drift/recovery,
tray + one global hotkey (mute master), egui mixer (master + group columns,
vertical faders, drag-drop app chips, device dropdowns, per-group settings
page), TOML hot-reload source-of-truth, autostart, single-instance.

## Competitor map

| Software | Edge over Splitstream |
|---|---|
| SteelSeries Sonar | Meters everywhere, per-app volume, EQ presets + graphical curve, **dual stream-vs-monitor mix**, AI mic noise-suppress, mic input chain |
| Voicemeeter Banana/Potato | Per-strip VU, gate/comp/EQ per strip, macro buttons (scene presets), VBAN network audio, hardware inserts, mic strips |
| EqualizerAPO + Peace | Graphical multi-band EQ, convolution loader, per-device |
| Audio Router / Chevolume | Closest twin (per-app device routing) — Splitstream already beats them |
| FxSound / Boom3D / Nahimic | Preset library, one-click profiles, branded polish, app-icon recognition |
| OBS advanced audio | Per-source monitoring ("listen to this"), VST host |

Pattern: rivals aren't deeper in audio — they're louder visually and cover mic
input + presets.

## Gap list, ranked by joy-per-effort

Priority 1 = do first. Effort is rough (engine-risk × UI-work).

| # | Gap | Why it matters | Effort | Status |
|---|---|---|---|---|
| ~~1~~ | ~~**Level meters**~~ | ~~Faders are blind today.~~ | Low | **✅ IMPLEMENTED 2026-07-22** — `level-meters.md` status complete; per-group vertical meters + master-column device list, tests green. Recommend `/review` next. |
| 2 | App icons on chips | Chips are bare text (often `game.exe`). Fetch exe icon, 16px. Instant recognition, big polish jump. | Low | **blueprint approved 2026-07-22** — `app-icons.md` at L4, ready for `/code-forge`. Introduces a new `win-shell` crate. |
| 3 | Per-group mute + solo | Only global mute exists. Standard mixer needs per-strip mute + solo. Config already per-group. | Low | **blueprint approved 2026-07-22** — `per-group-mute-solo.md` at L4, ready for `/code-forge`. |
| 4 | dB readouts + numeric entry + double-click-reset on faders | Faders are raw 0..1 linear taper. (Premise partly wrong — a number *and* typed entry already exist via egui's default `show_value`; the gap is the unit, the taper, and the unity snap. See `db-faders.md` Grounding.) | Low | **blueprint approved 2026-07-22** — `db-faders.md` at L4, ready for `/code-forge`. |
| 5 | Presets / profiles / scenes | No saveable profile. Gaming/Music/Streaming one-click switch via tray + hotkey. Leverages TOML-is-truth. | Med | **blueprint approved 2026-07-23** — `profiles.md` at L4, ready for `/code-forge`. Unblocks P9's profile-switch item and brings P10's multi-hotkey work forward. |
| 6 | Graphical multi-band EQ | Today one band per stage, no UI to add a band (no `AddEqBand` edit exists), sliders only. Rivals draw the curve + ship presets. Current EQ barely usable vs peers. (Premise refined: the **DSP is already multi-band** — `ParametricEq` cascades every configured band. Only the edit set and UI are missing.) | Med | **blueprint approved 2026-07-22** — `graphical-eq.md` at L4, ready for `/code-forge`. |
| 7 | Mic / input chain | Whole category absent — Splitstream is output-only (render + loopback). Capture + process mic (gate/noise-suppress) → virtual outputs for Discord/OBS. Biggest differentiator, biggest scope. **Confirm truly absent.** | High | queued |
| 8 | Streamer dual-mix | Sonar's killer feature: separate "what I hear" vs "what stream/Discord captures." Natural fit for the group model. | High | queued |
| 9 | Richer tray | Only mute/settings/quit. Add per-group quick-volume + profile switch. Many users live in tray. | Low | **blueprint approved 2026-07-23** — `external-controls.md` at L4 (per-group mute; profile submenu comes from `profiles.md`). Quick-volume dropped: superseded by the Windows volume binding, since a native menu can't hold a slider. |
| 10 | More hotkeys | Only `mute_master`. Add per-group vol/mute, profile-switch, push-to-mute. Infra exists. | Low | **blueprint approved 2026-07-23** — `external-controls.md` at L4. Push-to-mute confirmed feasible: `global-hotkey` 0.8 already delivers `Released` events, which today's code explicitly discards. |
| 11 | Visual identity | Default egui theme, no branding/accent, one custom widget. Dark/light toggle, accent, rounded meters = "product" vs "dev tool." | Med | **blueprint approved 2026-07-23** — `visual-identity.md` at L4, ready for `/code-forge`. Typography and a bundled font deliberately excluded as the *next* lever. |
| 12 | Session search/filter + empty-state guidance | Crowded chip pool when many apps open. Search box + empty states that teach the (currently undiscoverable) drag-drop. | Low | **blueprint approved 2026-07-22** — `session-search-and-guidance.md` at L4, ready for `/code-forge`. Scope grew: also adds a right-click assign menu. |

## Build order

All eight blueprinted features have a recommended implementation order with
dependency and merge notes in **`.lattice/ux-implementation-order.md`**. The
original per-gap sequencing below is superseded by it.

## Recommended sequencing (original, superseded)

1. **Visual trio first** (P1 meters + P2 icons + P3 mute/solo + P4 dB faders) —
   low engine risk, data mostly exists, transforms feel from "dev tool" to
   "product."
2. **Then one big differentiator** — mic chain (P7) OR streamer dual-mix (P8)
   OR presets (P5).

## Decisions

| Date | Decision |
|------|----------|
| 2026-07-22 | Level meters chosen as first feature to blueprint. Roadmap doc written before building (user pick). |
| 2026-07-23 | Visual-identity blueprint approved at L4 (`.lattice/context/visual-identity.md`). Dark/Light/Follow-system themes via `set_visuals_of`, a brand accent with contrast-verified presets (each carrying its own dark and light variant), a corner-radius/spacing style pass, the five hardcoded colours moved into named semantic entries, and a programmatic brand mark replacing the solid-blue `placeholder_icon`. Typography and a bundled font explicitly deferred. Ready for code-forge. |
| 2026-07-23 | External-controls blueprint approved at L4 (`.lattice/context/external-controls.md`), covering P9 + P10. **Scope changed mid-design by a user idea:** bind one group (or master) to the Windows default playback device's volume, so the hardware volume keys and the OS on-screen display drive a Splitstream fader — two-way, with `guidEventContext` supplying echo suppression. This supersedes the planned discrete tray volume levels (a native menu can't hold a slider), leaving the tray with per-group mute. Also: master/per-group volume and mute hotkeys, and push-to-mute. New `EndpointVolumePort` (a `SessionPort`-style separate trait). Ready for code-forge. |
| 2026-07-23 | Profiles blueprint approved at L4 (`.lattice/context/profiles.md`). `[[profile]]` tables inside config.toml capturing per-group state by name (incl. output_device and DSP, excl. match_rules); `[[group]]` stays the live state; explicit save with a computed modified indicator; tray + per-profile hotkey. Adds `ConfigEdit::SetDspChain` (no edit could replace a chain) and `control::edit_path` (the apply-path choice lived only at call sites). Ready for code-forge. |
| 2026-07-22 | Graphical-EQ blueprint approved at L4 (`.lattice/context/graphical-eq.md`). Peaking bells only (no new coefficient math), draggable curve handles, built-in presets, max 8 bands. Add/remove/preset collapse into one `SetEqBands` edit; `SetEqBand` gains a stage index (it silently targeted the first EQ stage). `EngineStats.group_rates` added — drawing at an assumed 48 kHz is ~3 dB wrong for treble bands on 44.1/128 kHz devices. Ready for code-forge. |
| 2026-07-22 | Session-search-and-guidance blueprint approved at L4 (`.lattice/context/session-search-and-guidance.md`). Global chip filter across every zone, four distinct empty states replacing one `(none)`, and a right-click assign menu funnelling into the existing `handle_drop`. Entirely `app::ui`. Ready for code-forge. **Note: overlaps `app-icons` in the `session_drop_zone` chip loop — whichever lands second must merge, not apply literally.** |
| 2026-07-22 | App-icons blueprint approved at L4 (`.lattice/context/app-icons.md`). Exe icons on session chips, extracted off-thread at 32 px and drawn at 16 pt, with a custom-painted initial-letter tile as both the pending and failure state. Introduces a new `win-shell` crate so the Win32 unsafe lands outside both `app` (zero-unsafe today) and `win-audio` (stays about audio). Ready for code-forge. |
| 2026-07-22 | dB-faders blueprint approved at L4 (`.lattice/context/db-faders.md`). Travel becomes linear in dB over −60…+6 (boost allowed), bottom of travel is true silence, dB readout + typed entry via egui's `custom_formatter`/`custom_parser`, double-click-to-unity via a click-sensing overlay. TOML stays linear — no schema change. Ready for code-forge. |
| 2026-07-22 | Per-group-mute-solo blueprint approved at L4 (`.lattice/context/per-group-mute-solo.md`). Mute persists to TOML on the proven `follow_master` path; solo is session-only, global across devices, UI-owned, cleared by a shell-published rebuild generation. Mute wins over solo; a silenced group stops triggering ducking. Ready for code-forge. |
| 2026-07-22 | Level-meters blueprint approved at L4 (`.lattice/context/level-meters.md`). Post-fader tap, peak+hold-dot, per-group + per-output (device list on master column), new `PeakMeter` in `audio-core/meter.rs`, `StatsReader` for per-frame poll, block-level `observe`. Ready for code-forge. Both open questions resolved: mic/input still deferred (P7); meter ballistics/tap settled. |

## Open questions

- Is a mic/input capture path truly absent, or partially present? Confirm before
  scoping P7.
- Meters: peak-only vs peak + RMS/VU ballistics? Pre-DSP or post-DSP tap point?
  Per-output too, or per-group only? (Resolve in the level-meters blueprint.)
