# Operational Learnings

Experiential patterns from practice. Complements standards (what should be) with experience (what we keep learning).

## Design Patterns

- 2026-07-18 [design] Interface-at-consumer for OS-API seams — when a crate wraps platform APIs, define port traits in the consumer crate, not the wrapper; otherwise the wrapper's platform deps break cross-platform tests.
- 2026-07-18 [design] Live control of RT systems splits cleanly: param changes via lock-free commands, structural changes via supervisor rebuild — the RT no-alloc/no-block rule decides which side a change falls on.
- 2026-07-18 [design] Keep third-party transport types (ring/channel libs) out of port trait signatures — pass slices, own the transport in the orchestrator; mocks stay trivial and the lib stays swappable.
- 2026-07-18 [design] Model feedback loops as pure `tick(measurements) -> commands` — unit-testable with synthetic curves, no threads or OS in the test.
- 2026-07-18 [design] Start control loops with only the directly-held error signal; add feedforward inputs (extra sensors/APIs) only when evidence shows slow convergence.
- 2026-07-18 [design] Extend an existing port facade with a method instead of adding a new single-consumer trait — a second abstraction needs a second consumer. Amended 2026-07-18: facade growth stops at concern boundaries — must-work and may-fail/feature-gated surfaces get separate traits even with one consumer each.
- 2026-07-18 [design] Manage external mutable state (OS prefs, remote config) as desired-state reconciliation with an applied-map — idempotent, re-runnable on any change, never rewrites what's already applied.
- 2026-07-18 [design] Trace latency through the whole mutation path before committing to single-funnel designs — file-watcher round-trips are 100s of ms; interactive controls need a fast path carrying the same value, source of truth unchanged.
- 2026-07-18 [design] Machine edits to user-editable files: semantic edit enum → format-preserving editor → atomic rename → echo suppression, as one package. Never raw-text patching, never re-serialize.
- 2026-07-18 [design] When a later design level invalidates an earlier decision, revise it explicitly with a logged revision entry — not silently. Keeps the decision log trustworthy.
- 2026-07-18 [design] Structural changes to RT-owned state: pre-build off-thread → pointer-swap via command → return old state for off-thread drop. Glitch-free, alloc-free on the hot path.
- 2026-07-18 [design] Processing that spans entities (sidechains, cross-feeds) doesn't fit per-entity pipeline traits — hoist to the orchestrator instead of widening the trait with params most stages ignore.
- 2026-07-18 [review] After designing multiple features against shared contracts, run a dedicated cross-blueprint seam review — event fan-out, channel producer counts, id stability across rebuilds, thread pacing. Per-feature reviews structurally miss these.
- 2026-07-19 [design] When implementation drops a spec'd domain field (e.g. `Format.layout`), capability silently narrows — the hard-fail surfaces at real-hardware boundaries, not in mocks, because mocks are built with matching formats. Diff implemented value objects against the spec's domain model when a gap appears.
- 2026-07-19 [design] When a feature's purpose is removing a failure class, make its core constructor infallible — fold unknown inputs with normalization rather than reintroducing strict errors at the edges (channel-mixdown: unknown speaker positions fold at −3 dB instead of erroring).
- 2026-07-19 [design] For format/audio conversion design, research the governing standard first (ITU-R BS.775, RFC 7845, FFmpeg's matrix model) — the standard's static-matrix formulation made RT-safety (pre-allocated, pure, infallible) fall out for free.

## Implementation Craft

- 2026-07-19 [implementation] When a blueprint's contract references a type documented under a sibling component that the build order constructs later, relocate the type to the earlier-built *consumer* rather than reordering the build or fully decoupling — same interface-at-consumer idiom as port traits, extended to config/DTO types.
- 2026-07-19 [implementation] For unfamiliar or fast-moving external crate APIs (major-version churn), verify exact signatures by writing the call and reading the real compiler error, not from memory or pre-written notes — notes can predate a dependency's breaking change; the compiler is ground truth. When a crate's own docs aren't browsable in-session, this is also faster than hunting for docs.
- 2026-07-19 [implementation] Before committing to a dependency's latest major version, check for accidental package-name collisions in its transitive tree (an external crate can happen to share your own workspace crate's name) — breaks `cargo build/test -p <name>`. Pinning to an older minor that matches already-written internal documentation can dodge both the collision and an API rewrite at once.
- 2026-07-19 [implementation] RAII guards for OS thread-affine resources (COM apartments, etc.) can't be struct fields on a type required to be `Send + Sync` — use a lazy per-thread `thread_local!` initializer called at the top of every method that touches the resource instead.
- 2026-07-19 [implementation] Real-time thread orchestration functions (open ports → compute timing → build the mixer → spawn N thread kinds → assemble a handle) accrete responsibilities fast and quietly cross the parameter-count/size thresholds — `#[allow(clippy::too_many_arguments)]` on a hot function is a signal to extract a context struct right then, not a thing to suppress and revisit later. Caught one review cycle later than it should have been.
- 2026-07-19 [implementation] A design's L3 interaction chain can name components a later, unbuilt phase owns (e.g. "gain → DspChain → duck → matrix → SRC" when DspChain/duck are P5 and not yet in the repo). Check the actual current code before implementing, not just the blueprint text — slot the new stage where the future component will insert, rather than stubbing a hook for it or blocking the whole feature on the other phase landing first.
- 2026-07-19 [implementation] Before threading a new field through a domain type (e.g. adding `layout` to `Format`), grep for every site that constructs that type raw from an external source — duplicated derivation logic (three places independently rebuilding `Format` from a raw `WAVEFORMATEX*`) is invisible in a blueprint that names only one of them, and adding new parsing logic to all three independently triples the chance of getting risky unsafe code wrong in exactly one spot. Consolidate to one shared function first, then add the field.
- 2026-07-19 [implementation] Existing `#[ignore]`d real-hardware tests are a normal implementation-time validation step for platform-boundary features, not optional polish saved for later — running one against real hardware mid-implementation caught in seconds what a mock structurally cannot (e.g. confirming a mask-decode path against the actual device that motivated the feature), turning a "should work" into a "confirmed on the real device."

## Quality Signals

- 2026-07-19 [review] In a fallback/precedence table written as `match` arms, an unguarded arm always wins once its pattern matches — it silently bypasses a shared catch-all that sibling arms fall through to correctly. Inconsistent guarding across otherwise-parallel arms (3 of 11 speaker-fold rules unguarded, 8 correctly guarded) let one function's own documented "never lose audio" contract quietly break for a subset of valid inputs. When reviewing or writing this pattern, check every arm for the same guard-vs-unguarded shape, not just a sample of them.

## Reliability

- 2026-07-18 [design] For unreliable/undocumented APIs: first failure → flag + single notice + skip further calls; retry only on deliberate user action. No retry storms, no silent spam.
- 2026-07-18 [design] Cross-feeding computations: fix producer-before-consumer order within a cycle/block and reject dependency cycles at validation time — determinism by construction, not runtime checks.
- 2026-07-19 [reliability] Real hardware/OS smoke tests (marked ignored, run explicitly during development, not in normal CI) catch API-shape and integration mistakes that mocks structurally cannot — worth writing per platform-boundary component even when the permanent suite can't run them unattended.

## Structural Health
