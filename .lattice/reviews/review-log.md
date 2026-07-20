# Review Log

## 2026-07-19 — engine-core (P0–P1), full implementation
- **Scope**: ~20 files across 5 crates, all layers (domain/application/infrastructure/shell)
- **Atoms**: clean-code, architecture, domain-driven-design, secure-coding, test-quality
- **Result**: 0 critical, 2 warning, 5 suggestion — all fixed
- **Key findings**: `build_running_graph` bundled 5 responsibilities in one function; `mixer_loop` took 9 params behind a suppressed clippy lint; a test name claimed behavior it didn't assert
- **Strengths**: clean acyclic dependency graph (verified via `cargo tree`), consistent RT-safety discipline with documented `unsafe impl Send` invariants, real-hardware smoke tests alongside 35 unit/integration tests

## 2026-07-19 — channel-mixdown, full implementation
- **Scope**: 10 files across audio-core/win-audio/engine, domain+infra+application layers
- **Atoms**: clean-code, architecture, domain-driven-design, secure-coding, test-quality
- **Result**: 0 critical, 1 warning, 0 suggestion — fixed
- **Key findings**: `fold_targets`'s FL/FR/FC arms were unguarded, silently dropping audio for output layouts lacking all three, unlike the correctly-guarded BL/SL/BR/SR arms
- **Strengths**: real-hardware validation against the exact motivating device (SteelSeries Sonar "Media" → `SURROUND_7_1`), zero-signature-change reuse of `Src`, DRY consolidation of a 3-site duplicated unsafe WASAPI parse
