# Review Log

## History
- 2026-07-22 level-meters (full implementation) — 0 critical/1 warning/3 suggestion, warning fixed; `OutputId`-order mislabeling after a parked group fixed with a real name map
- 2026-07-22 responsive-ui-refinement — 1 critical/warning borderline/0/1, both fixed; concave speaker-cone polygon (epaint convex-only) split into two convex calls
- 2026-07-22 session-mute-on-capture — 0/0/0, clean; all 4 L3 flows plus the negative case covered
- 2026-07-22 mixer-ui-redesign (drag-and-drop) — 0/1/1, warning confirmed intentional, suggestion fixed
- 2026-07-22 process-loopback-capture (architecture pivot) — 0/1/1, spec-doc BYOD-still-described drift + `Dispatcher` double-lock fixed
- 2026-07-21 simple-launch (installer) — 0/0/0, clean; Inno Setup flags verified against live docs
- 2026-07-21 simple-launch (onboarding UI) — 0/1/0, `output_device`/`bus_endpoint` collision on first rebuild fixed
- 2026-07-21 simple-launch (control+app infra) — 0/1/2, blocking-call-under-shared-lock + duplicated `write_atomic` fixed
- 2026-07-19 engine-core (P0-P1) — 0/2/5, all fixed; `build_running_graph` SRP split + `mixer_loop` param-count cleanup
- 2026-07-19 channel-mixdown — 0/1/0, unguarded FL/FR/FC fold arms fixed, real-hardware validated

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

## 2026-07-23 — per-group-mute-solo, full implementation
- **Scope**: 9 files (no new files) across audio-core/engine/control/app, domain+orchestration+control+shell+UI layers
- **Atoms**: clean-code, architecture, domain-driven-design, test-quality (secure-coding skipped — no new trust-boundary surface)
- **Result**: 0 critical, 1 warning, 2 suggestion — **all 3 fixed same session** (M/S colors now read `ui.visuals().error_fg_color`/`warn_fg_color`; `dimmed_by_other_solo` extracted to pure `is_dimmed_by_other_solo` + 4 new tests; store.rs test renamed to match sibling round-trip naming)
- **Key findings**: `MUTE_ACTIVE_COLOR`/`SOLO_ACTIVE_COLOR` are new hardcoded `Color32` constants instead of theme-derived `ui.visuals()` — directly materializes a risk visual-identity.md's own learning predicted for exactly this kind of unbuilt feature; `dimmed_by_other_solo` (the mute-excludes-dim precedence) computed inline, uncovered by any test, unlike sibling `clear_solo_on_rebuild` which was deliberately extracted pure for testability; a store.rs round-trip test's name doesn't match its two sibling tests' naming convention for the identical set-then-unset shape.
- **Strengths**: all 5 layers match the L2/L4 blueprint contracts exactly, zero scope creep; `mix_tick`'s RT change (solo_active derived once/tick, reused phase 1+3 per decision 6; ballistics-always-advance/trigger-env-forced-down split) checked carefully, no new locks/allocs, no dependency-direction changes; every blueprint test contract present plus one symmetric bonus test; 290 workspace tests green, clippy clean throughout

## 2026-07-23 — db-faders, full implementation
- **Scope**: 3 files (audio-core: dsp.rs, lib.rs; app: ui.rs), domain + UI layers
- **Atoms**: clean-code, architecture, domain-driven-design, test-quality (secure-coding skipped — no trust boundary touched)
- **Result**: 0 critical, 1 warning, 1 suggestion — **warning fixed same session**, suggestion left open
- **Key findings**: the single load-bearing guarantee added this feature (an out-of-range hand-written gain must render truthfully, not get silently squashed) had zero regression coverage; the first fix attempt (a live-frame `egui::__run_test_ui` test asserting `.changed()==false`) was empirically non-discriminating — `Slider::get_value()` pre-clamps on every read under `Always` mode, so `changed()` never fires regardless of clamping mode. Replaced with a pure test on `gain_to_fader_db` directly. Also corrected the design doc's decision 10, which cited the wrong egui mechanism for why the fix was needed (see operational learnings). `fader_db_to_gain`'s `.expect()` depends on an invariant only its one caller currently upholds (left open, low risk).
- **Strengths**: pure mapping (`fader_db_to_gain`/`gain_to_fader_db`/`format_fader_db`/`parse_fader_db`) fully split from widget code, same discipline as level-meters' helpers; zero new crate edges; the mid-session self-caught pre-clamp bug held up under independent empirical re-verification even though the design doc's stated reasoning for it didn't; 298 workspace tests green, clippy clean throughout

## 2026-07-23 — session-search-and-guidance, full implementation
- **Scope**: 1 file (crates/app/src/ui.rs), UI layer only
- **Atoms**: clean-code, test-quality (architecture/DDD/secure-coding skipped — single file, no domain/trust-boundary touch)
- **Result**: 1 critical, 1 suggestion — **both fixed same session**
- **Key findings**: capability 3's "Esc clears the search" was dead code — egui's `Memory::begin_frame` clears focus globally on an unclaimed Escape *before* the widget renders that frame, so `response.has_focus()` reads false exactly when Escape fires; confirmed with a two-frame `egui::Context` repro, fixed to `lost_focus()`, added a regression test verified discriminating against the bug. A stale doc comment from the old `session_drop_zone(sessions) -> Option<u32>` signature was left orphaned above `enum AssignChoice`, describing the wrong item; removed.
- **Strengths**: click-sense-overlay idiom (chip context menu) cleanly reused the pattern already validated by the fader's reset gesture; all 9 planned pure test contracts present including the easy-to-miss `zone_had_chips` case; zero scope creep, `handle_drop`/`resolve_drag_assign` reused untouched; 315 workspace tests green, clippy clean throughout

## 2026-07-23 — app-icons, full implementation
- **Scope**: 6 files (new `win-shell` crate, new `app::icons` module, `app::ui` integration, workspace/Cargo.toml wiring) — platform + shell + UI layers
- **Atoms**: clean-code, architecture, secure-coding, test-quality (DDD skipped — no domain/aggregate touch)
- **Result**: 1 warning — **fixed same session**
- **Key findings**: `IconCache`'s two most load-bearing guarantees (decision 4 path-keyed dedup, decision 5 negative caching — a failed path never retried) had zero test coverage despite being pure `HashMap`-state assertions with no Win32 dependency; added `a_failed_extraction_is_never_retried` and `a_pending_path_is_not_enqueued_twice`, both verified discriminating by breaking the guard and confirming the test caught it. Independently re-verified (not re-trusted) the full unsafe `extract_icon_rgba` pipeline against the pinned `windows` 0.62.2 source and the `ChipZoneCtx`/`&mut IconCache` borrow-check correction — both held up.
- **Strengths**: RAII cleanup reuses the library's own `Owned<T: Free>` instead of hand-rolled guards; `IconSlot` correctly stays private, collapsing pending/failed/no-path into one `None` at the call site; the mid-forge empirical BMP-dump visual verification against a real icon gave unusually high confidence in the one part of this feature no test could reach; zero engine/control/audio-core touch, single new crate edge (`app -> win-shell`); 322 workspace tests green, clippy clean throughout

## 2026-07-23 — graphical-eq, full implementation
- **Scope**: 8 files across audio-core/engine/control/app — domain + orchestration + control + shell + UI layers (biggest feature this session-block, all 4 crates)
- **Atoms**: clean-code, architecture, domain-driven-design, test-quality (secure-coding skipped — no new trust boundary)
- **Result**: 1 warning — **fixed same session**
- **Key findings**: `hit_test_handle` (nearest-handle-within-radius, the function every drag/click/scroll/remove routes through) had zero test coverage despite needing no egui frame — no test contract in the design doc covered it either. Added 3 tests, verified discriminating (swapped `min_by`/`max_by`, confirmed the nearest-picking test caught the swap). Independently re-traced whether `press_origin()`/`smooth_scroll_delta` being frame-global rather than per-widget could cross-contaminate multiple EQ stages' editors — confirmed safe, `hover_pos()` and the click/drag response flags are correctly per-widget-scoped.
- **Strengths**: `eq_response_db` built directly on the real `Biquad::set_coeffs_peaking`, pinned by a test that measures an actual filtered sine wave rather than re-checking internal consistency; decision 13's stage-index bug fixed and regression-tested at both the store and shell layers independently; `SetEqBands` replaces the whole array rather than mutating in place, sidestepping the inline-array trap by construction rather than by reuse; zero scope creep across the largest diff this session block; 340 workspace tests green, clippy clean throughout

## 2026-07-24 — mixer-demand-driven-wakeup, full implementation
- **Scope**: 1 file (crates/engine/src/runtime.rs), engine orchestration layer only
- **Atoms**: clean-code, test-quality (architecture/DDD/secure-coding skipped — single file/layer, no domain/trust-boundary touch)
- **Result**: 1 warning, 1 suggestion — **both fixed same session**
- **Key findings**: `stop_running_graph` set the mixer's `stop` flag but never called the new `MixerWaker::wake()`, so `EngineHandle::shutdown`/`apply_rebuild` (including automatic device-fault recovery) silently regressed from ~5-10ms join latency to up to `MIXER_FALLBACK_INTERVAL` (100ms) whenever nothing else happened to wake the mixer — a shutdown/rebuild exit path none of the design's Flows A-I named; fixed with one `wake()` call plus a new regression test (`stopping_a_parked_mixer_joins_promptly...`). `CaptureControl::apply_capture_sources` cloned `mixer_waker` once per pid inside a loop instead of once before it; hoisted.
- **Strengths**: `mixer_loop`'s two hardest-to-test contracts (ticks immediately before any wake; drains every pending source in one tick) correctly asserted against `RingGauge.active` bookkeeping rather than mixed audio content, sidestepping the real `Src` resampler's genuine warm-up latency rather than fighting it; `park`/`unpark`'s token-persistence used for race-free, bounded wake-regression tests (`park_timeout` + elapsed-time assertion, never a bare hang-risk `park()`); 103/103 engine tests + full workspace green, clippy clean throughout
