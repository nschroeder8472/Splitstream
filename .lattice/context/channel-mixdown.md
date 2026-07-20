---
feature: channel-mixdown
requirement_doc: Splitstream-Engineering-Spec.md
created: 2026-07-19
status: complete
---

# Channel Mixdown

> Gap found during P2 hardware smoke test: no downmix/upmix exists anywhere; a group whose bus channel count differs from its output's hard-fails at `Src::new` (`DomainError::ChannelMismatch`). Common real case: 8-channel bus (SteelSeries Sonar "Media") or 5.1/7.1 movie source routed to stereo headphones. This feature designs N→M channel conversion.

## Research (2026-07-19)

- **ITU-R BS.775 stereo downmix:** `L = FL + 0.7071·C + 0.7071·Ls`, `R = FR + 0.7071·C + 0.7071·Rs`; LFE discarded by default.
- **General N→M:** static mixing matrix `out = M[out_ch × in_ch] · in`, built once from (input layout, output layout). Same-speaker pass-through 1.0; center → L/R at −3 dB; surrounds fold to same-side front at −3 dB; back-center splits; LFE dropped (configurable); mono→stereo duplicate; stereo→mono 0.5·(L+R). Normalize rows summing > 1.0 (clipping guard). Upmix = pass-through, no synthetic surround. (FFmpeg swresample / RFC 7845 model.)
- **Prerequisite:** channel *layout* (speaker positions), not just count. Spec's domain model lists `Format { sample_rate, channels, layout }` — implementation dropped `layout`. WASAPI provides `dwChannelMask` via `WAVEFORMATEXTENSIBLE`.
- Sources: ITU-R BS.775-3, FFmpeg channel-layout docs, RFC 7845 downmix section.

## Decisions Log

<!-- Add new at bottom. Never remove. -->

| Date | Decision | Reasoning | Alternatives Considered |
|------|----------|-----------|------------------------|
| 2026-07-19 | LFE discarded in downmix. | ITU-R BS.775 standard default; standard-mixed content carries bass in mains; no boom/clip risk on headphones. | Fixed −6 dB mix-in (FFmpeg lfe_mix_level); per-group TOML knob (config surface for niche need — retrofittable). |
| 2026-07-19 | `Format` gains `layout: ChannelLayout` field; invariant `layout.count() == channels` validated at construction. | Matches spec's original domain model (`Format { sample_rate, channels, layout }`); existing `fmt.channels` call sites untouched. | Layout replaces `channels` (derived method — most pure, most churn); layout outside `Format` on specs only (invariant lives nowhere, drift risk). |
| 2026-07-19 | Matrix placement fixed: between duck and SRC, always. | Downmix before resample — SRC runs at output channel count, cheaper for common many→2; DSP/duck stay at source layout. Upmix pays trivial extra SRC work — not worth two code paths. | Matrix after SRC (SRC at input width — dearer for downmix); adaptive placement (two code paths for negligible win); fused into Src (couples two orthogonal concerns). |
| 2026-07-19 | No matrix trait, no separate builder component. Coefficient generation folds into `ChannelMatrix::new`. | One concrete type, one consumer — single-consumer abstraction rule (operational learnings). | `ChannelMap` trait + builder (speculative flexibility). |
| 2026-07-19 | Unknown/unmapped input speaker positions fold into **all** outputs at −3 dB; construction never fails. | Audio never silently lost; capability 1 ("never hard-fails") holds universally; row normalization still guards clipping. | Discard unmapped channels (height content vanishes); hard-fail (reintroduces the ChannelMismatch failure class this feature kills). |
| 2026-07-19 | No new MixerCommand; layout changes ride existing structural rebuild. | Nothing user-tunable at runtime — LFE fixed, coefficients standard; epoch mechanism already covers staleness. | Live SetChannelMatrix command (no use case). |
| 2026-07-19 | Design approved at Level 4. Status set to approved — ready for implementation. | All four levels approved and persisted; drift check vs spec complete (3 entries recorded in spec ## Links). | — |
| 2026-07-19 | **Implementation-time decision (code-forge):** matrix stage placed directly after gain (`gain·master → matrix → SRC → accumulate`), not literally between `DspChain`/duck and SRC as L3 prose reads. | `DspStage`/`Ducker` (P5) aren't built yet in this codebase — verified against real `mixer.rs` before implementing. Matrix occupies the exact slot P5 will insert before when it lands; zero rework expected. | Stub/no-op DspChain hook now (speculative, nothing to hook into yet); wait for P5 to land first (blocks this feature indefinitely, P5 isn't next in `implementation-order.md`). |
| 2026-07-19 | **Implementation-time decision (code-forge):** extracted a shared `format_from_wfx(*const WAVEFORMATEX) -> Format` helper in `win-audio/format.rs`; `capture.rs` and `render.rs` call it instead of re-deriving `Format` from the raw wfx pointer themselves. | Found by grep, not in the blueprint: `capture.rs`/`render.rs` each independently built `Format` from `*wfx` — three sites duplicating the same unsafe cast, only one (`format.rs`) named in the L4 contract. Adding a mask-aware layout read to all three independently would have meant getting risky unsafe pointer logic right three times. | Duplicate the mask-read in all three sites (triples the risk of the exact unsafe-cast mistake notes §17 warns about). |
| 2026-07-19 | **Implementation-time decision (code-forge):** added `log_channel_conversions` in `engine/runtime.rs`, called once per graph build (off-RT) — fulfills L3 interaction D ("supervisor logs per group downmix"), which the L4 contract section didn't carry a concrete signature for. | Engine crate had zero logging infrastructure (`log`/`tracing` not a dependency yet) — used a plain `println!` matching `app/src/main.rs`'s existing pattern rather than introducing a new dependency for one line. | Add `tracing` crate now (scope creep — no other logging need yet); skip the log entirely (silently drops an approved L3 interaction). |

## Implementation Notes (code-forge, 2026-07-19)

- **Real-hardware validation**: ran the existing `#[ignore]`d `enumerate_real_render_endpoints` test (win-audio) against this machine's actual devices. "SteelSeries Sonar - Media" (the exact endpoint from the original bug report) reports `dwChannelMask = 1599` (0x63F) — decoded to precisely `ChannelLayout::SURROUND_7_1` (not a count-fallback guess). Confirms the mask-read path works correctly on the real motivating hardware, not just synthetic tests.
- **`Src`'s `ChannelMismatch` check needed zero signature changes** — the L4 contract's "Src relax" component turned out to require no code changes to `resample.rs` at all. `Mixer::build_group` now always constructs both `Src` `Format`s at the output channel count; the existing equal-channels check simply becomes unreachable from any public path, exactly as designed. Kept the check and its test (`channel_mismatch_is_rejected`) as `Src`'s own internal invariant.
- **`ChannelMatrix`'s fold-rule table is not exhaustive** — explicit rules cover FL/FR/FC/LFE/BL/BR/SL/SR/BC/FLC/FRC; anything else (height channels, unrecognized bits) falls through to the unknown-position catch-all (fold into every output at −3 dB). This is a deliberate simplification within the approved "never lose audio" capability, not a gap — exotic layouts get a safe, standards-adjacent fallback rather than a hand-tuned rule.
- **`speaker` module in `sample.rs` is `pub(crate)`**, not private — `channel.rs` is a sibling module (not a descendant of `sample`), so it needs crate-level visibility to read the WASAPI bit constants. `ChannelLayout::speakers()` is `pub(crate)` for the same reason.
- All new/changed code: `cargo build --workspace`, `cargo test --workspace` (60 tests, all pass), `cargo clippy --workspace --all-targets` (zero warnings) — clean.

## Review (2026-07-19)

`/review` found and fixed one warning: `fold_targets`'s FL/FR/FC fallback arms (`channel.rs`) were unguarded, so when a speaker's own fallback target was also absent from the output layout, the arm matched unconditionally and returned an empty contribution — bypassing the unknown-position catch-all that BL/SL/BR/SR correctly fall through to. Fixed by guarding all three arms (`if has(FC)` / `if has(FL) || has(FR)`); added regression test `center_channel_reaches_output_even_when_layout_has_no_front_speakers_at_all`. See `.lattice/reviews/review-log.md`.

## Design: Level 1 -- Capabilities

Approved 2026-07-19.

1. **Channel-count mismatch never hard-fails** — any bus channel count routes to any output channel count; `ChannelMismatch` construction failure gone for supported layouts.
2. **Standards-correct downmix** — multichannel (5.1/7.1/quad/etc.) to stereo/mono per ITU-R BS.775: center/surrounds at −3 dB, matrix normalized against clipping.
3. **Pass-through upmix** — fewer→more: matching speakers pass through, extra outputs silent (mono→stereo duplicates). No synthetic surround.
4. **Layout-aware, count-fallback** — real speaker layout (WASAPI channel mask) when available; unknown mask infers standard layout from count.
5. **RT-safe and live** — matrix pre-allocated at graph build; layout changes ride existing structural rebuild; zero hot-path alloc.

Out of scope: synthetic surround upmix, per-group custom matrix TOML config, LFE bass management/crossover, HRTF/virtualization.

## Design: Level 2 -- Components

Approved 2026-07-19.

| Component | Home / layer | Single responsibility |
|---|---|---|
| `ChannelLayout` | `audio-core/sample.rs` (domain) | Value object: speaker-position set, WASAPI-mask-compatible bit order; `default_for_count(n)` fallback (1=mono, 2=stereo, 6=5.1, 8=7.1…) |
| `ChannelMatrix` | `audio-core/channel.rs` new (domain) | Pre-allocated M×N coefficient matrix + `process(in, out)`; built from (in layout, out layout) via BS.775 rules + normalization; identity = skip |
| Mixer integration | `audio-core/mixer.rs` | `push_group` chain: gain → DspChain (P5) → duck → **matrix** → SRC → accumulate; per-group pre-allocated matrixed scratch |
| `Src` relax | `audio-core/resample.rs` | Mixer constructs `Src` with equal channel counts post-matrix; `ChannelMismatch` stays internal invariant, no longer user-reachable |
| Layout probe | `win-audio/format.rs` | `client_mix_format()` reads `WAVEFORMATEXTENSIBLE.dwChannelMask` → `ChannelLayout`; zero/unknown mask → count fallback |
| Graph plumbing | `engine/graph.rs`, `ports` | `Endpoint`/`Format` carry layout into `GroupSpec`/`OutputSpec`; `Mixer::new` no longer fails on count mismatch |

```mermaid
graph LR
    G[gain·master] --> CH[DspChain P5] --> DK[duck] --> CM[ChannelMatrix N→M] --> SRC[SRC @ M ch] --> ACC[output accumulator]
```

DDD: `ChannelLayout` immutable value object inside `Format`; `ChannelMatrix` pure domain computation, no trait (one concrete type, one consumer).

## Design: Level 3 -- Interactions

Approved 2026-07-19.

**A — Graph build (supervisor, off-RT):** enumerator reports `Endpoint.format` with layout (`win-audio` reads `dwChannelMask`; zero/unknown mask → `ChannelLayout::default_for_count`). `graph::resolve` carries layouts into `GroupSpec.input_format` / `OutputSpec.format`. `Mixer::new` per group builds `ChannelMatrix::new(in_layout, out_layout)` — identity when equal (stage skipped); `Src` at output channel count both sides; matrixed scratch pre-allocated (`max_block_frames × out_ch`).

**B — Steady state (RT, per `push_group`):** gain·master → DspChain → duck (all at source layout) → matrix `out[f][m] = Σₙ coef[m][n]·in[f][n]` over pre-allocated scratch → SRC → accumulate. Identity: matrix skipped, zero extra copy.

**C — Layout change (device swap / driver reconfig):** existing structural-rebuild path; supervisor rebuilds matrix off-thread. No new MixerCommand.

**D — Build logging:** supervisor logs per group `"group X: 8ch → 2ch downmix"`; no EngineStats change.

**E — Unknown speaker positions:** fold into all outputs at −3 dB; construction never fails (see Decisions Log).

## Design: Level 4 -- Contracts

Approved 2026-07-19. Deltas on approved P0–P5 contracts; signatures only.

### `audio-core` — sample.rs

```rust
/// Speaker-position set. Bit order = WASAPI dwChannelMask (SPEAKER_FRONT_LEFT = 0x1, …).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChannelLayout(u32);

impl ChannelLayout {
    pub const MONO: Self;            // FC
    pub const STEREO: Self;          // FL|FR
    pub const QUAD: Self;            // FL|FR|BL|BR
    pub const SURROUND_5_1: Self;    // FL|FR|FC|LFE|BL|BR
    pub const SURROUND_7_1: Self;    // 5.1 + SL|SR

    /// mask==0 or popcount(mask) != channels → default_for_count.
    pub fn from_mask(mask: u32, channels: u16) -> ChannelLayout;
    /// 1=mono, 2=stereo, 3=FL/FR/FC, 4=quad, 5=5.0, 6=5.1, 7=6.1, 8=7.1;
    /// >8 → first 8 known + rest unknown-position (fold rule applies).
    pub fn default_for_count(channels: u16) -> ChannelLayout;
    pub fn count(&self) -> u16;
}

pub struct Format { pub sample_rate: u32, pub channels: u16, pub layout: ChannelLayout }
// fields stay pub (tests/mocks build literals); boundary constructors guarantee sanity;
// Mixer::new validates layout.count() == channels → DomainError::InvalidLayout

pub enum DomainError { /* existing… */ InvalidLayout { channels: u16, layout_count: u16 } }
// ChannelMismatch retained — internal Src invariant only, unreachable from public paths
```

### `audio-core` — channel.rs (new)

```rust
pub struct ChannelMatrix { /* coef: M×N row-major, pre-allocated; identity flag */ }
impl ChannelMatrix {
    /// Infallible. BS.775 rules: pass-through 1.0; C→L/R −3 dB; surrounds→same-side −3 dB;
    /// back-center splits; LFE dropped; unknown positions → all outputs −3 dB;
    /// rows normalized if sum > 1.0. from == to → identity.
    pub fn new(from: ChannelLayout, to: ChannelLayout) -> ChannelMatrix;
    pub fn is_identity(&self) -> bool;
    /// input: whole frames at N ch; output holds same frame count at M ch. Returns samples written.
    pub fn process(&self, input: &[f32], output: &mut [f32]) -> usize;
}
```

### `audio-core` — mixer.rs

```rust
// GroupState gains: matrix: ChannelMatrix, matrixed: Vec<f32>  (max_block_frames × out_ch)
// push_group chain: gain → DspChain → duck → matrix (skip if identity) → SRC → accumulate
// Mixer::new: Src built at output channel count both sides; group/output count mismatch no longer an error
```

### `win-audio` — format.rs

```rust
// client_mix_format(): WAVEFORMATEXTENSIBLE → Format { rate, channels, from_mask(dwChannelMask, channels) }
// plain WAVEFORMATEX (no mask) → default_for_count
```

### `engine` / `control`

No signature changes. `Endpoint`/`GroupSpec`/`OutputSpec` carry layout inside `Format`. `control` untouched — no channel config in TOML. Supervisor logs downmix at build.

## Open Questions

None — all resolved during design (placement, layout representation, LFE, unknown masks/positions — see Decisions Log).

## Constraints

Inherited (binding): pure DSP in `audio-core` (no OS, CI-testable); RT threads never alloc/lock/block — matrix pre-allocated at graph build; f32 interleaved; live changes via lock-free commands or supervisor rebuild; COM/WASAPI confined to `win-audio`.

## Design Summary

- **Components/layers:** `ChannelLayout` value object (`audio-core/sample.rs`), `ChannelMatrix` (`audio-core/channel.rs`, new — pure domain), mixer chain integration (`push_group`: gain → DspChain → duck → matrix → SRC → accumulate), layout probe in `win-audio/format.rs` (`dwChannelMask`), layout plumbed through `engine::graph` inside `Format`.
- **Key contracts:** `ChannelLayout::{from_mask, default_for_count, count}` + layout consts; `Format` gains `layout` field (fields stay pub, boundary constructors guarantee sanity); `ChannelMatrix::{new (infallible), is_identity, process}`; `DomainError::InvalidLayout`; no engine/control signature changes; no new MixerCommand.
- **Architectural constraints:** matrix pre-allocated at graph build, pure loop on RT path, zero hot-path alloc; layout changes via existing structural rebuild; COM mask-reading confined to win-audio.
- **Domain decisions:** ITU-R BS.775 coefficients (C/surrounds −3 dB), LFE discarded, pass-through upmix (no synthetic surround), unknown speaker positions fold into all outputs at −3 dB, rows normalized against clipping, construction never fails — kills the `ChannelMismatch` failure class.
- **Resolved during design:** LFE policy, layout home (`Format` field), matrix placement (before SRC), unknown-layout policy, no-trait decision.
- Drift check vs `Splitstream-Engineering-Spec.md` complete — §8 override, §6.1 alignment + addition recorded in spec `## Links`.

## Key Files

| Path | Role |
|---|---|
| Splitstream-Engineering-Spec.md | Requirement spec (`Format` domain model mentions `layout`) |
| crates/audio-core/src/sample.rs | `ChannelLayout`, `Format` (gained `layout`), `DomainError::{ChannelMismatch,InvalidLayout}` |
| crates/audio-core/src/channel.rs | `ChannelMatrix` — new, fold-rule table + normalization + `process()` |
| crates/audio-core/src/resample.rs | `Src::new` — `ChannelMismatch` now an unreachable-from-public-path internal invariant |
| crates/audio-core/src/mixer.rs | `Mixer::{build_group,push_group}` — matrix stage wired between gain and SRC |
| crates/win-audio/src/format.rs | `format_from_wfx()` — new shared mask-aware `Format` builder |
| crates/win-audio/src/capture.rs | `open()` — now calls `format_from_wfx` instead of duplicating it |
| crates/win-audio/src/render.rs | `open()` — now calls `format_from_wfx` instead of duplicating it |
| crates/engine/src/runtime.rs | `log_channel_conversions()` — new, build-time downmix visibility (L3 interaction D) |
| .lattice/context/engine-core.md | Approved P0–P1 contracts this extends |
