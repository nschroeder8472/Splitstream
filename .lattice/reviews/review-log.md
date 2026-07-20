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

## 2026-07-20 — drift-and-recovery (P2), full implementation
- **Scope**: 12 files across engine/win-audio, application+infrastructure layers
- **Atoms**: clean-code, architecture, secure-coding, test-quality (DDD skipped — no domain files touched)
- **Result**: 1 critical, 0 warning, 1 suggestion — both fixed same session
- **Key findings**: `OnDeviceAdded` called `IMMDeviceEnumerator::GetDevice` synchronously inside the `IMMNotificationClient` callback — MSDN-documented deadlock risk on the OS's shared notification thread; fixed by deferring the describe work to a spawned worker thread. `handle_device_added` lacked the duplicate-notification guard `handle_endpoint_lost` has; fixed by deduping `added_endpoints` by id per supervisor tick, same pattern as the existing `dead_endpoints` dedup.
- **Strengths**: engine layer matched the approved L4 blueprint exactly; 87 tests passing including real-hardware validation of `default_output()` against live WASAPI; clean RAII unregister-on-drop for the COM notification lifetime

## 2026-07-20 — drift-and-recovery (P2), follow-up review of uncommitted diff
- **Scope**: 13 files (11 modified + clock.rs/monitor.rs new) across engine/win-audio, same P2 feature re-reviewed pre-commit
- **Atoms**: clean-code, architecture, secure-coding, test-quality (DDD skipped — no domain files touched)
- **Result**: 0 critical, 1 warning, 4 suggestion — none fixed yet (all polish-level, left for author)
- **Key findings**: `render_loop` carries 7 raw params while sibling `capture_loop` was refactored to a context struct in this same diff for exactly that reason; fixed-`sleep` before emitting mock device events in supervisor integration tests is a latent CI-flakiness seed; multi-fault-per-tick triggers N sequential rebuilds instead of batching (undocumented, unlike the codebase's other accepted rebuild race)
- **Strengths**: `cargo check`/`cargo test --workspace`/`cargo clippy` all clean (44/44 engine tests passing); drift PI loop stays pure and unit-tested via synthetic curves; COM callback correctly defers to a worker thread, matching this project's own documented deadlock learning
