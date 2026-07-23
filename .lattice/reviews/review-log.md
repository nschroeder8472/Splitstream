# Review Log

## 2026-07-22 — level-meters, full implementation
- **Scope**: 5 files (audio-core meter.rs new + mixer.rs taps, engine runtime.rs telemetry + StatsReader, engine graph.rs output-name map, app ui.rs widgets + main.rs handoff) — domain + engine + shell
- **Atoms**: clean-code, architecture, DDD, test-quality (secure-coding skipped — read-only telemetry, no trust boundary)
- **Result**: 0 critical, 1 warning, 3 suggestion — labels-warning fixed same session; re-review of working tree confirmed no correctness/concurrency defects; open items (widget DRY between `level_meter`/`output_meter_row`, unbounded `holds` map, per-frame double-clone of stats vecs into locals) left as optional
- **Key findings**: master-column output-meter labels reproduced the engine's `OutputId` order from `snapshot.groups`, which mislabels every device after a parked group during a device-loss episode (values correct, names wrong) — fixed by exposing a real `OutputId → name` map from `graph::resolve` through `EngineStats.output_names`; regression test added for the parked-earlier-group case
- **Strengths**: rides the existing RT-atomic→EngineStats→poll telemetry path with zero new transport (domain stays transport-free); idle-freeze trap caught with `observe_silence`; peak+clip packed into one `AtomicU64` (no torn pair); egui 0.35 painter APIs verified against vendored source before writing; strong AAA tests incl. silence-paths-agree invariant

## 2026-07-22 — responsive-ui-refinement, full implementation
- **Scope**: 3 files (ui.rs main delta, lifecycle.rs/logging.rs minor fixes), single shell layer
- **Atoms**: clean-code, test-quality (architecture/DDD/secure-coding skipped — single-file/layer, no domain, no trust boundary)
- **Result**: 1 critical/warning borderline, 0 warning, 1 suggestion — both fixed same session
- **Key findings**: `speaker_mute_button`'s cone was one 6-point `convex_polygon` call, but the combined body+horn outline has a reflex vertex at the seam (verified via cross-product sign flip: `-42,+140,+140,-42,+36,+36`) — epaint's fill is convex-only, so the concave shape rendered incorrectly; split into two convex calls (rect body + horn quad). `Screen`'s unused `PartialEq` derive dropped (only `matches!` used, never `==`).
- **Strengths**: every L4 contract implemented with zero deviation from approved design; 8 new unit tests hit both clamp bounds plus pass-through/zero-column edge cases; `Shape`/`Painter` constructor signatures verified against vendored egui 0.35 source rather than memory; `cargo clippy -p app --tests -- -D warnings` and 39/39 tests clean throughout

## 2026-07-22 — session-mute-on-capture, full implementation
- **Scope**: 4 files (ports/mod.rs, ports/mock.rs, routing.rs, win-audio/sessions.rs) — application+infrastructure layers
- **Atoms**: clean-code, architecture, secure-coding, test-quality (DDD skipped — no domain-folder files touched)
- **Result**: 0 critical, 0 warning, 0 suggestion
- **Key findings**: none survived — matches L4 contracts verbatim, mute strictly follows confirmed capture success, mute failure correctly isolated from `RoutingDegraded`
- **Strengths**: all 4 L3 flows plus the negative (failed-capture-never-muted) case covered by tests; 94/94 engine tests + full workspace build clean

## 2026-07-22 — mixer-ui-redesign, settings window column layout + drag-and-drop
- **Scope**: 4 files (routing.rs, event_pump.rs, main.rs wiring, ui.rs full rewrite), engine+shell layers
- **Atoms**: clean-code, architecture, test-quality
- **Result**: 0 critical, 1 warning (resolved: confirmed intentional), 1 suggestion (fixed)
- **Key findings**: Master's Mute checkbox moved behind gear icon by inferred symmetry, not mockup-dictated — user confirmed keep-as-is; one test's input data never actually exercised the glob-rule-survives-unassign path it claimed to test
- **Strengths**: `resolve_drag_assign`'s full target×has_exact branch matrix independently tested; egui 0.35 dnd API verified against real vendored source, zero compile-fix churn across all 3 layers

## 2026-07-22 — process-loopback-capture, full architecture pivot
- **Scope**: 22 files (4 new, 2 deleted) across engine/win-audio/control/app, all layers
- **Atoms**: clean-code, architecture, secure-coding, test-quality
- **Result**: 0 critical, 1 warning, 1 suggestion
- **Key findings**: spec `## Links` says §9.5 (virtual driver) is eliminated, but §2.2/§9.5 body/§13/§15 open-question text still describes BYOD as active; `Dispatcher::set_current` double-locks `ui` mutex unnecessarily
- **Strengths**: async completion-callback given a bounded timeout, lock scope kept off the blocking WASAPI activation call, and a dead-capture-thread reap/retry path — all three real-hardware review findings from the prior session, already fixed before this review; `cargo check`/`cargo test --workspace` clean (87+ engine tests)

## 2026-07-21 — simple-launch, installer/splitstream.iss
- **Scope**: 1 file (new), build-artifact layer (no code)
- **Atoms**: clean-code, architecture, secure-coding
- **Result**: 0 critical, 0 warning, 0 suggestion
- **Key findings**: none survived — verified `ArchitecturesInstallIn64BitMode=x64compatible`, `RunOnceId`, GUID escaping against live docs rather than memory
- **Strengths**: `runasoriginaluser` correctly used on both `[Run]` and `[UninstallRun]` for the elevated-installer/de-elevated-app split

## 2026-07-21 — simple-launch, onboarding UI + bus-classification plumbing
- **Scope**: 8 files (ui.rs, engine::AudioSystem/AppConfig, win-audio enumerator/monitor/system/sessions/lib) across domain/application/shell layers
- **Atoms**: clean-code, architecture, secure-coding, test-quality
- **Result**: 0 critical, 1 warning, 0 suggestion — fixed
- **Key findings**: onboarding's `output_device` fallback could collide with the picked `bus_endpoint` (OS default output already being the virtual cable), silently failing the first post-onboarding rebuild
- **Strengths**: proactively reused the prior review's "never hold a shared lock across a blocking call" fix in the new `BusMatch` enumerator code, unprompted

## 2026-07-21 — simple-launch, control + app infra layers (in-progress)
- **Scope**: 7 files (2 new) across control/app, application+shell layers
- **Atoms**: clean-code, architecture, secure-coding, test-quality
- **Result**: 0 critical, 1 warning, 2 suggestion — all fixed
- **Key findings**: `Dispatcher::set_current` held the per-frame UI mutex across a blocking WASAPI `enumerate()` call; `write_atomic` duplicated verbatim between `config.rs`/`store.rs`; seed template's `schema_version` was a second hardcoded literal
- **Strengths**: reused one `sys.enumerate()` call across `needs_onboarding`+`available_devices` instead of duplicating it; fatal-vs-non-fatal startup error classification matches the blueprint's Flow 5 exactly

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

## 2026-07-21 — process-loopback-capture, full implementation
- **Scope**: 24 files (2 new, 2 deleted) across engine/win-audio/control/app, application+infrastructure+shell layers
- **Atoms**: clean-code, architecture, secure-coding, test-quality (DDD skipped — no domain-folder files touched)
- **Result**: 1 critical/warning borderline, 1 warning, 1 suggestion — all 3 fixed same session
- **Key findings**: `CaptureControl::apply_capture_sources` held the engine's shared running-graph lock across a blocking, unbounded-wait WASAPI COM activation call — the 3rd occurrence of this project's "blocking call under a shared lock" shape, invisible to mock-backed tests; restructured to open ports unlocked, added a timeout to the underlying async wait. A pid whose capture thread died mid-stream (not at open) was never reaped, permanently zombied instead of retried per the L3 "retried every time" design intent — fixed with an `is_finished()` reap pass, proven by a new regression test. `process_capture::open`'s ~120-line body split at its activation/initialize seam.
- **Strengths**: real-hardware validation against a live pid caught two independent bugs (a `STATUS_HEAP_CORRUPTION` from a `PROPVARIANT`/`ManuallyDrop` interaction, and `GetMixFormat` returning `E_NOTIMPL` on this client type) before either shipped, both documented with the real error and fix reasoning in the context doc; 224 workspace tests green throughout, including a new mid-stream-death regression test that fails against the pre-fix code

## 2026-07-20 — spatial-audio, full implementation
- **Scope**: 12 files (10 modified + spatial.rs/hrir_data.rs new) across audio-core/control/engine/app, domain+application+infrastructure+shell layers
- **Atoms**: clean-code, architecture, domain-driven-design, test-quality (secure-coding skipped — no new trust-boundary surface)
- **Result**: 0 critical, 1 warning, 2 suggestion — all 3 fixed same session
- **Key findings**: `log_channel_conversions` called `HrirSet::embedded(rate)` a second time purely to read `.taps()` for a log line, duplicating the real construction `Render::build` already did for the same group — fixed via new pure `HrirSet::taps_for(rate)`; a doc comment in `hrir_data.rs` referenced a nonexistent `synth::ear_pair` (actual fn is the top-level `synth_pair`) — fixed; `Spatializer::process` was missing the interleaved-length `debug_assert` its sibling `ChannelMatrix::process` has — added
- **Strengths**: `PartitionedConvolver`'s FDL overlap-save algorithm verified against 3 hand-derived closed-form test cases (unit-impulse identity, 2-tap average, 2-partition/BRIR-shaped reconstruction) before any real audio path touched it; `Render::build` fallback rule shared cleanly between the off-thread graph-build and live-toggle paths with zero duplication; full workspace (audio-core 70, control 43, engine 88, app 15) green throughout all 4 layers

## 2026-07-22 — level-meters, full implementation (unreviewed working tree)
- **Scope**: 10 files (meter.rs new) across audio-core/engine/app, domain+orchestration+shell+UI layers
- **Atoms**: clean-code, architecture, domain-driven-design, test-quality (secure-coding skipped — no trust-boundary surface in the delta)
- **Result**: 0 critical, 3 warning (1 severity-borderline), 5 suggestion — **all 3 warnings fixed same session** (lock hoist; `PeakMeter` flush-to-zero + 2 regression tests; 12-element tuple replaced by a named `Frame` struct carrying `EngineStats` whole). All 5 suggestions remain open. 280 workspace tests green, clippy clean.
- **Key findings**: `ui.rs`'s per-frame `state.stats = self.stats.stats()` sits inside the `self.ui.lock()` scope while `stats()` takes the engine's `running` mutex that `apply_rebuild` holds across blocking WASAPI device opens — **4th occurrence** of this project's blocking-call-under-a-shared-lock shape, fix is hoisting the read above the lock. `PeakMeter`'s envelope now advances through silence (the frozen-bar fix) and therefore decays into permanent subnormal floats on the RT thread — needs a flush-to-zero guard. The frame-data destructure reached a 12-element tuple in the same file that had just had two ctx structs extracted for parameter-count creep.
- **Strengths**: `paint_meter` shares one painter between the vertical fader meter and horizontal device row with `vertical` as the only axis difference, so scale/color/hold/clip semantics structurally cannot drift; `graph.rs`'s `output_devices` fixes parked-group mislabeling at the authoritative side with a test naming the exact failure case; `encode_meter` packs peak+clip into one `AtomicU64` so the pair cannot tear, with `0` decoding to `SILENT`
