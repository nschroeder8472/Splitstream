---
feature: session-routing
requirement_doc: Splitstream-Engineering-Spec.md
created: 2026-07-18
status: approved
---

# Session Routing (P3)

> P3 — session enumeration, per-app→bus routing via undocumented APIs, endpoint hiding. Exit criteria: apps auto-assign to groups by rule; extra devices hidden; graceful fallback verified (spec §13).

## Decisions Log

<!-- Add new at bottom. Never remove. -->

| Date | Decision | Reasoning | Alternatives Considered |
|------|----------|-----------|------------------------|
| 2026-07-18 | Blueprint scope = P3: session enumeration (F5), per-app→bus routing (F6), endpoint hiding/default (F7). Builds on approved engine-core + drift-and-recovery. | Spec §13 phase order; P0–P2 designs approved. | P4 shell; P5 DSP. |
| 2026-07-18 | Separate `SessionPort`/`PolicyPort` traits in engine::ports — not grown onto `AudioSystem`. Deliberate exception to grow-facade learning. | Distinct concern cluster: best-effort, feature-gated, may fail without killing audio. Keeps AudioSystem cohesive; mocks simulate policy failure independently. | Growing AudioSystem (blurs must-work vs may-fail degradation posture). |
| 2026-07-18 | Match rules: process image name + full path, glob syntax. Precedence: exact name > glob; ties → first group in config order. Window title dropped. **Resolves spec §15.5.** | Spec config example already glob-shaped. Titles volatile — rematch churn on retitle, extra Win32 surface. Specific-first prevents early catch-all swallowing exact rules. | Config-order-only (catch-all foot-gun); including window title (churn + surface). |
| 2026-07-18 | No un-route on session end; persisted Windows prefs left in place. Un-route (`clear_route`) only when rules change makes an app unmatched. | Re-launch races new-session notification — first seconds of audio would hit wrong device. Windows-persisted pref gives correct routing from first sample. | Clearing route on every session end (cleaner Windows state, audible race). |
| 2026-07-18 | Degradation: first PolicyPort failure sets degraded flag + one `RoutingDegraded` notice; further policy calls skipped; retry only on config reload. | N4 graceful degradation without retry storms; reload = deliberate user action. | Periodic auto-retry (storm risk on permanently-broken API); fail-hard (violates N4). |
| 2026-07-18 | `ConfigDelta` gains `Rules` variant — control-plane change class, distinct from Params/Structural. | Rule edits must not rebuild audio graph nor touch mixer; they re-run reconciliation only. | Treating rule change as Structural (needless audio gap). |
| 2026-07-18 | PolicyRouter behind cargo feature `policy-routing`; unavailable/disabled surfaces as `PolicyError::Unavailable` → immediate degraded mode. | Spec §9 risk note: isolate + feature-gate undocumented surfaces; single code path for "feature off" and "API broke". | Runtime-only detection without build-time gate (can't ship a build with the surface fully absent). |
| 2026-07-18 | Design approved at Level 4. Status set to approved — ready for implementation. | All four levels approved and persisted; drift check vs spec complete. | — |
| 2026-07-18 | **Revision (cross-blueprint review):** new flow + contract `RoutingHandle::update_topology(buses, rules)` — called after every structural rebuild. Coordinator re-enumerates sessions and reconciles desired state against the fresh (possibly re-numbered) GroupIds; applied-routes map rebuilt. | L3 only handled rules changes; group add/remove/rename never reached the coordinator — stale `buses` map and applied-map keyed by shifted positional GroupIds. Desired-state reconcile pattern already in place makes this cheap. | Restarting the coordinator on structural change (loses degraded flag + notification dedup state); persistent group ids (rejected in P4). |
| 2026-07-18 | **Revision (cross-blueprint review):** `SessionPort::events(&self)` becomes `take_events(&mut self) -> Receiver<SessionEvent>` — single-consume, same fix as EngineHandle. | Same single-consumer Receiver issue found in the seam review. | — |

## Design: Level 1 -- Capabilities

Approved 2026-07-18.

1. **Apps auto-assign to groups** — app starts playing → routed to matching group's bus; applies to running and new sessions.
2. **Unmatched apps stay normal** — no rule match → branded default, no surprise routing.
3. **Clean Windows device list** — bus endpoints hidden from sound UI; branded default set.
4. **Graceful degradation** — undocumented surfaces fail → audio keeps flowing, polish lost, user told once; no crash, no retry storm.
5. **Live rule updates** — config rule edits re-route affected apps without restart.

Out of scope: rule-editing UI (P4), DSP (P5), capture-device exposure (§15.6).

## Design: Level 2 -- Components

Approved 2026-07-18. Control-plane only; audio path untouched.

| Component | Home / layer | Single responsibility |
|---|---|---|
| RuleMatcher | `control/rules.rs` (application, pure) | `(pid, process_path, display_name) × rules → Option<GroupId>` |
| SessionPort + PolicyPort | `engine::ports` | Session enumeration/notifications; route-session + visibility/default (separate trait — best-effort concern) |
| RoutingCoordinator | `engine/routing.rs` (application) | Desired-state reconciliation; re-apply on rule/session change; degradation posture (one notice, no retry storm); bus hiding at startup |
| WasapiSessions + PolicyRouter | `win-audio` (infrastructure) | IAudioSessionManager2 + priming (§9.2); hand-declared AudioPolicyConfig/IPolicyConfig vtables, feature-gated (§9.3–9.4) |

```mermaid
graph TD
    CFG[control: match_rules] --> RM[control: RuleMatcher]
    RM --> RC[engine: RoutingCoordinator]
    WS[win-audio: WasapiSessions] -->|SessionEvent| RC
    RC -->|route/hide via PolicyPort| PR[win-audio: PolicyRouter]
    RC -->|EngineEvent::RoutingDegraded| APP[app]
```

Endpoint-visibility manager folded into RoutingCoordinator (single caller). RuleMatcher = pure function, no service.

## Design: Level 3 -- Interactions

Approved 2026-07-18.

**A — Startup reconcile:** coordinator hides non-default buses + sets branded default (failure → E) → `SessionPort::enumerate()` (primes notifications §9.2) → match each session → `route(pid, bus)` for matched; unmatched untouched. Applied-routes map avoids rewriting persisted prefs.

**B — New session:** notification → match → route if matched and not already applied.

**C — Session ended:** drop from map; no un-route (Windows persists per-app pref; same group next launch).

**D — Live rule change:** `ConfigDelta::Rules` (new variant — control-plane, neither param nor structural) → re-match all live sessions → route newly-matched, re-route changed, `clear_route(pid)` for now-unmatched (back to default).

**E — Degradation:** first PolicyPort failure → degraded flag, `EngineEvent::RoutingDegraded{reason}` once, skip all further policy calls; retry only on config reload. Audio path never affected.

**F — Shutdown:** endpoints stay hidden (Windows persists; uninstaller restores). No un-routing.

## Design: Level 4 -- Contracts

Approved 2026-07-18. Deltas on approved P0–P2 contracts; signatures only.

### `control`

```rust
pub enum MatchRule { ExactName(String), Glob(GlobPattern) }
pub struct GroupRules { pub group: GroupId, pub rules: Vec<MatchRule> }
pub struct SessionInfo { pub pid: u32, pub process_path: PathBuf, pub display_name: String }

/// Exact name > glob; ties broken by config order. None → leave on default.
pub fn match_session(info: &SessionInfo, rules: &[GroupRules]) -> Option<GroupId>;

pub enum ConfigDelta { Params(Vec<MixerCommand>), Rules(Vec<GroupRules>), Structural, Unchanged }
```

### `engine::ports` additions (win-audio implements)

```rust
pub enum SessionEvent { New(SessionInfo), Ended(u32) }

pub trait SessionPort: Send {
    fn enumerate(&mut self) -> Result<Vec<SessionInfo>, PortError>;   // must prime notifications (§9.2)
    fn events(&self) -> Receiver<SessionEvent>;
}

pub enum PolicyError { Unavailable(String), Failed(String) }

pub trait PolicyPort: Send {                                          // all best-effort (§9.3–9.4)
    fn route(&mut self, pid: u32, bus: &EndpointId) -> Result<(), PolicyError>;
    fn clear_route(&mut self, pid: u32) -> Result<(), PolicyError>;
    fn set_visibility(&mut self, endpoint: &EndpointId, visible: bool) -> Result<(), PolicyError>;
    fn set_default(&mut self, endpoint: &EndpointId) -> Result<(), PolicyError>;
}
```

### `engine/routing.rs`

```rust
pub fn start_routing(rules: Vec<GroupRules>, buses: HashMap<GroupId, EndpointId>,
                     session: Box<dyn SessionPort>, policy: Box<dyn PolicyPort>,
                     events: Sender<EngineEvent>) -> Result<RoutingHandle, EngineError>;

impl RoutingHandle {
    pub fn update_rules(&self, rules: Vec<GroupRules>);
    pub fn is_degraded(&self) -> bool;
    pub fn shutdown(self);
}

pub enum EngineEvent { /* P2 variants… */ RoutingDegraded { reason: String } }
```

### `win-audio`

`WasapiSessions: SessionPort`; `PolicyRouter: PolicyPort` behind cargo feature `policy-routing` — hand-declared vtables; feature off or activation failure → `PolicyError::Unavailable` → degraded at startup, audio unaffected.

## Open Questions

None — §15.5 rule precedence resolved (see Decisions Log).

## Constraints

Inherited (binding): COM in win-audio only; port traits in engine (interface-at-consumer); no exclusive mode; RT constraints unchanged (session routing is control-plane only — never touches audio path).

P3-specific (spec §9.2–9.4, N4):
- §9.3 `AudioPolicyConfig`/`SetPersistedDefaultAudioEndpoint` and §9.4 `IPolicyConfig` are **undocumented** — hand-declared vtables, best-effort with fallback + logging; app must keep routing audio (less polish) if they fail.
- Feature-gate + isolate both undocumented surfaces.
- New-session notifications require priming: call `GetSessionEnumerator` at least once on the manager during init (§9.2 gotcha).
- No elevation — undocumented COM runs in user context (§12).

## Design Revisions (2026-07-18 cross-blueprint review)

```rust
impl RoutingHandle {
    /// Call after every structural rebuild: fresh bus map + rules; coordinator
    /// re-enumerates sessions and reconciles (applied-map rebuilt, degraded flag preserved).
    pub fn update_topology(&self, buses: HashMap<GroupId, EndpointId>, rules: Vec<GroupRules>);
}
pub trait SessionPort: Send {
    fn enumerate(&mut self) -> Result<Vec<SessionInfo>, PortError>;
    fn take_events(&mut self) -> Receiver<SessionEvent>;   // replaces events(&self); single-consume
}
```

New flow **H — Structural rebuild:** engine completes rebuild → app calls `update_topology` with new buses/rules → coordinator reconciles as in flow A (idempotent; already-correct persisted routes untouched).

## Design Summary

- **Components/layers:** `RuleMatcher` pure fn (`control/rules.rs`); `SessionPort` + `PolicyPort` traits (`engine::ports`); `RoutingCoordinator` (`engine/routing.rs`, desired-state reconciliation + degradation + bus hiding); `WasapiSessions` + `PolicyRouter` (`win-audio`, feature-gated).
- **Key contracts:** `match_session(info, rules) -> Option<GroupId>`; `SessionPort::{enumerate, events}`; `PolicyPort::{route, clear_route, set_visibility, set_default}` (all best-effort, `PolicyError`); `start_routing`/`RoutingHandle::{update_rules, is_degraded}`; `ConfigDelta::Rules`; `EngineEvent::RoutingDegraded`.
- **Architectural constraints:** control-plane only — audio path untouched; undocumented surfaces isolated in win-audio behind `policy-routing` feature; one degradation notice, retry only on config reload; no elevation.
- **Domain decisions:** `MatchRule` value objects (validated globs); exact > glob > config-order precedence (resolves spec §15.5); persisted routes left in place on session end.
- **Resolved during design:** §15.5 rule precedence; port shape (separate PolicyPort — deliberate exception to grow-facade learning); un-route semantics; degradation/retry policy; feature gate.
- Drift check vs `Splitstream-Engineering-Spec.md` complete — see spec `## Links`.

## Key Files

| Path | Role |
|---|---|
| Splitstream-Engineering-Spec.md | Requirement spec (§9.2–9.4, F5–F7, N4, §15.5) |
| .lattice/context/engine-core.md | Approved P0–P1 design (ports facade, control plane, ConfigSnapshot.match_rules) |
| .lattice/context/drift-and-recovery.md | Approved P2 design (EngineEvent channel, supervisor patterns) |
