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

## Implementation Craft

## Quality Signals

## Reliability

- 2026-07-18 [design] For unreliable/undocumented APIs: first failure → flag + single notice + skip further calls; retry only on deliberate user action. No retry storms, no silent spam.
- 2026-07-18 [design] Cross-feeding computations: fix producer-before-consumer order within a cycle/block and reject dependency cycles at validation time — determinism by construction, not runtime checks.

## Structural Health
