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

## 2026-07-20 — session-routing (P3), full implementation
- **Scope**: 16 files (12 modified + 4 new) across engine/control/win-audio/app, application+infrastructure+shell layers
- **Atoms**: clean-code, architecture, secure-coding, test-quality (DDD skipped — no domain files touched)
- **Result**: 0 critical, 1 warning, 1 suggestion — both fixed same session
- **Key findings**: `WasapiSessions` never called `UnregisterAudioSessionNotification`/`UnregisterSessionNotification`, leaking OS-side COM registrations indefinitely — same shape `DeviceMonitor` (P2) already solved but not replicated here; fixed via explicit `Drop` on new `SessionRegistration`/`ManagerRegistration` wrappers. Inert `let _keep_alive = &ctx.session;` in `routing.rs` removed (did nothing — `ctx` already owns `session` for the function's whole scope); field renamed `_session` to keep the real keep-alive documented without a fake mechanism.
- **Strengths**: undocumented `IPolicyConfig`/`AudioPolicyConfig` COM verified live against EarTrumpet's actual source rather than the repo's own pattern sketch, which caught the sketch undercounting the real vtable by 10 slots before it shipped; `ConfigDelta`'s enum→struct restructure ships with a regression test proving the original silent-drop bug is actually fixed; all 9 routing-coordinator tests exercise the real background thread against mocks, covering all 8 L3 flows

## 2026-07-20 — dsp-pipeline (P5), in-progress diff (audio-core + engine layers)
- **Scope**: 10 files (8 modified + dsp.rs/smoothing.rs new) across audio-core/engine/control/app, domain+application layers (control/app touched only mechanically)
- **Atoms**: clean-code, architecture, domain-driven-design, test-quality (secure-coding skipped — no trust-boundary code in this delta)
- **Result**: 3 critical, 1 warning, 2 suggestion — all fixed same session
- **Key findings**: `EnvFollower`, `DuckTargetGain`, and `BypassRamp` each stepped a per-sample one-pole smoother once per interleaved element instead of once per frame, making duck/bypass timing scale inversely with channel count (Nx too fast); `EqBand::recompute`'s smoother advanced once per 32-frame sub-block instead of once per frame in it, making EQ param ramps 32x slower than documented. All four share one root cause and one fix shape (advance once per frame, reuse across channels) already correct elsewhere in the same file.
- **Strengths**: RBJ peaking-EQ coefficients and TDF2 form match the cookbook exactly; zero-gain-EQ-is-an-exact-identity gives a deterministic FFT-free test; the mixer's two-phase tick split correctly satisfies the design doc's hardest ordering constraint (every duck trigger's envelope before any target's gain, within one block)

## 2026-07-20 — dsp-pipeline (P5), in-progress diff (control + app layers)
- **Scope**: 5 files (config.rs/store.rs/main.rs/ui.rs modified, no new files) across control/app, infrastructure+shell layers
- **Atoms**: clean-code, architecture, domain-driven-design, test-quality (secure-coding skipped — no trust-boundary code in this delta)
- **Result**: 1 critical, 0 warning, 0 suggestion — fixed same session
- **Key findings**: `store.rs`'s `dsp_array`/`bands`/`duck` TOML mutation helpers used `.expect()` assuming an array-of-tables/table on-disk shape; a hand-written but equally-valid inline shape (`bands = [{...}]`, `dsp = [{...}]`, `duck = {...}`) parses fine at `ConfigStore::open` but panicked the whole app on the first `SetEqBand`/`AddDspStage`/`SetDuck` edit against it. Confirmed by direct reproduction before fixing; converted to `StoreError::Validation` at all three sites.
- **Strengths**: `diff()`'s three-way dsp_chains/bypass-only/duck branching correctly avoids an unnecessary chain rebuild for a pure bypass toggle; duck cycle detection and unknown-trigger validation both run at config-parse time (fail fast, before ever reaching the engine); 39/39 control tests and 85/85 engine tests passing throughout
