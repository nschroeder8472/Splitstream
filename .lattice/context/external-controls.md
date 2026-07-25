---
feature: external-controls
requirement_doc: null
created: 2026-07-23
status: complete
note: >
  Roadmap Priorities 9 and 10 (minus what profiles already brings), plus a
  system-volume binding raised by the user mid-design that supersedes the
  originally-planned tray volume levels. Reaching the mixer without opening
  the window: Windows volume binding, tray mute, hotkeys, push-to-mute.
  Originally drafted as `tray-and-hotkeys`; renamed once binding became the
  centerpiece.
---

# External Controls

> Three ways to reach the mixer without opening it. The centerpiece: bind one
> group (or master) to the Windows default playback device's volume, so the
> keyboard volume keys and the OS on-screen display drive a Splitstream fader
> directly. Around it, per-group mute in the tray and hotkeys for the rest.

## Grounding (2026-07-23, pre-Level-1)

### Hotkeys and tray

- **Key release already arrives and is discarded.** `global-hotkey` 0.8 defines
  `HotKeyState::{Pressed, Released}` (lib.rs:66) and its Windows backend emits
  `Released` (platform_impl/windows/mod.rs:162). `run_hotkeys` filters
  `evt.state == HotKeyState::Pressed` explicitly, so push-to-mute needs no new
  mechanism — only to stop discarding the other half.
- **Hotkey registration is singular today.** `spawn_hotkeys` reads
  `map.mute_master` and registers exactly one `HotKey`. The profiles blueprint
  already generalizes this to N chords; this feature consumes that rather than
  repeating it.
- **`muda` 0.19.3 supports the menu shapes needed** — `Submenu` with runtime
  `append`/`remove_at`/`set_text`, `CheckMenuItem` with `set_checked`.
- **A native menu cannot host a slider**, a platform constraint rather than a
  design preference — which is what made the binding idea worth taking over the
  originally-planned discrete tray levels.
- **The tray menu is built once.** `run_tray` calls `build_menu()` at startup
  with a fixed `TrayIds { mute, settings, quit }` and accepts only
  `TrayCommand::{Quit, Notice}`; per-group entries need a rebuild command.

### System volume binding

- **Every API needed is present and already enabled.**
  `IAudioEndpointVolume::{GetMasterVolumeLevelScalar, SetMasterVolumeLevelScalar}`
  and `RegisterControlChangeNotify` (windows 0.62.2,
  `Win32_Media_Audio_Endpoints` — already in `win-audio`'s feature list), with a
  **generated** `IAudioEndpointVolumeCallback_Impl` trait, so `#[implement]`
  works normally here rather than needing a hand-declared vtable like
  `IPolicyConfig` did.
- **Notifications carry their own echo suppression.**
  `AUDIO_VOLUME_NOTIFICATION_DATA { guidEventContext, bMuted, fMasterVolume, .. }`
  — passing our own GUID to `SetMasterVolumeLevelScalar` and ignoring
  notifications bearing it is the documented way to avoid a feedback loop. Same
  shape as `ConfigStore::is_echo`, but supplied by the API.
- **Mute rides the same notification** (`bMuted`), so the mute key drives the
  bound target's mute at no extra cost.
- **Media keys act on the *default playback device's* endpoint volume**, and
  that endpoint's volume is applied by the OS to everything rendered to it.
  Two consequences:
  - If a group's output device *is* the default device, **Windows volume keys
    already control that group today** — the endpoint attenuates the stream.
    Mirroring the same level into the group's gain would apply it twice
    (50% → 25%).
  - Binding is therefore useful, and safe, exactly when the bound target
    renders to a **non-default** device: the keys move a level nothing else
    consumes, and we spend it driving the bound target.
- **Default-device changes are already observable.** `DeviceMonitor`
  (drift-and-recovery) implements `IMMNotificationClient`, so re-subscribing on
  a default change reuses an existing mechanism, including the logged
  windows-core direct-dependency gotcha for `#[implement]`.

## Design: Level 1 -- Capabilities

**Approved 2026-07-23.**

### A. System volume binding

1. **One bindable target at a time** — a single group, or master, bound to the
   Windows default playback device's volume. Optional, off by default; unbound
   behaves exactly as today.
2. **Windows -> Splitstream** — endpoint volume changes from the volume keys,
   the OSD, or the OS mixer slider set the bound target's gain.
3. **Splitstream -> Windows** — moving the bound target's fader sets the
   endpoint volume, so the OSD always agrees with the app. Our own writes are
   ignored on the way back via `guidEventContext`.
4. **Mute follows too** — the mute key toggles the bound target's mute, from
   the `bMuted` field of the same notification.
5. **Default-device changes re-subscribe automatically**, reusing
   `DeviceMonitor`'s existing `IMMNotificationClient`.
6. **Double-attenuation guard** — when the bound target's own output device *is*
   the current default device, mirroring is suspended and the UI says why:
   Windows is already attenuating that audio natively, so mirroring would square
   the attenuation. This can arise at any moment, since the user can change
   their default device.
7. **Failure is inert** — a failed subscription or lost callback is logged and
   leaves the target's fader working normally. Audio is never affected.

### B. Tray

8. **Per-group submenu with mute only** — discrete volume levels dropped;
   binding supersedes them.
9. **The tray tracks the group set** — adding, removing or renaming a group
   rebuilds the submenu list rather than leaving stale entries.

### C. Hotkeys

10. **Master volume up/down**, 3 dB per step.
11. **Per-group volume up/down**, same step. Still useful for groups that are
    not the bound one.
12. **Per-group mute toggle** (needs `GroupConfig.muted` from the pending
    per-group-mute-solo blueprint).
13. **Push-to-mute master** — mute while held, restore on release.
14. **Push-to-mute restores the *prior* state, not "unmuted"** — holding while
    already muted must leave it muted.
15. **Push-to-mute can never strand the audio muted** — a missed `Released`
    auto-restores after a bounded max-hold, and the next `Pressed` re-arms from
    actual state.
16. **Bindings live with what they control**, are all optional, fail inertly,
    and steps clamp rather than wrap.

Out of scope (v1): push-to-*talk* (a mic concept — P7, unbuilt); binding more
than one target at once; binding to a non-default endpoint; a hotkey-editing UI
(chords stay config-file-only); per-output or solo hotkeys; profile-switch
hotkeys (owned by the profiles blueprint); whether a profile captures which
target is bound — noted as a follow-up rather than designed here.

## Decisions Log

| # | Date | Decision | Reasoning / alternatives rejected |
|---|------|----------|-----------------------------------|
| 1 | 2026-07-23 | **A system volume binding supersedes the planned discrete tray volume levels.** Raised by the user mid-design, after Level 1 had already been drafted around tray levels. | A native menu cannot host a slider, so tray volume was always going to be a coarse subset of a real control. Binding gives continuous control *plus* the native OSD, using hardware keys the user already reaches for. The tray keeps per-group mute, which stays genuinely useful. Rejected: keeping both (two parallel volume mechanisms to build and explain, with the tray half still coarse). |
| 2 | 2026-07-23 | **The bound target follows the current default playback device**, re-subscribing when it changes. | That is exactly what the volume keys and the OSD act on, so the binding tracks user intent with no configuration. Rejected: a user-picked endpoint (explicit and stable, and double-attenuation becomes unreachable by construction — but the volume keys would not act on it, removing most of the convenience). Consequence: the double-attenuation case becomes reachable at runtime, requiring decision 4's guard. |
| 3 | 2026-07-23 | **Two-way sync**, with `guidEventContext` suppressing our own echo. | One-way would desync the moment the user moves the Splitstream fader, and the next key press would snap the value back — reading as the fader being overridden. Two-way keeps the app and the OSD in agreement. The echo mechanism is supplied by the API rather than invented, so this is not the fragile part it would otherwise be. Accepted cost: we write to a system-wide endpoint volume, so a bug here is visible outside the app. |
| 4 | 2026-07-23 | **Mirroring suspends when the bound target's output device is the current default device.** | The endpoint volume is applied by the OS to everything rendered to that device, so the bound target's audio is *already* attenuated by exactly the value being mirrored; applying it again squares it (50% → 25%). In that configuration the volume keys already work natively, so suspending the mirror is not a loss of function — it is the correct behaviour, and the UI should say so rather than appearing broken. |
| 5 | 2026-07-23 | **Mute follows the same binding**, from `bMuted` on the existing notification. | The data is already in the payload; ignoring it would mean the mute key silently does nothing to the bound target while the volume keys work. |
| 6 | 2026-07-23 | **3 dB per hotkey step, clamped at the ends.** | A recognisable "one notch" in audio; crossing the useful range takes about seven presses rather than twenty. Clamping rather than wrapping, because a volume control that jumps from silent to full on one press is a hazard. |
| 7 | 2026-07-23 | **Push-to-mute restores the prior state on release, and is protected by a bounded max-hold rather than focus tracking.** | Restoring to "unmuted" would turn the hold gesture into a silent unmute whenever master was already muted. The safety net matters because a missed `Released` (focus loss, stuck key, swallowed event) would otherwise strand the audio muted with no indication of why; a timeout needs no OS hooks and fails safe, where focus tracking is more precise but requires machinery this app does not have. |
| 8 | 2026-07-23 | **Per-group chords live on the group's own table; master and global chords stay in `[hotkeys]`.** | Matches how the profiles blueprint puts `hotkey` on the profile table — a binding lives with the thing it controls, so deleting the thing deletes its binding. |
| 9 | 2026-07-23 | **Feature renamed from `tray-and-hotkeys` to `external-controls`.** | Once binding became the centerpiece, the original name described the smallest part of the feature. Renamed while the document was minutes old and had no dependents. |
| 10 | 2026-07-23 | **Endpoint volume is a separate `EndpointVolumePort` trait, obtained via an `AudioSystem::open_default_endpoint_volume()` facade method.** | Follows `SessionPort`'s documented rationale rather than the grow-the-facade default: it is a distinct, best-effort, possibly-unavailable concern (a device may not support volume control) with its own owner. A trait rather than a concrete type so the reconcile logic is testable against a mock without COM, which is the same reason `MockSystem` exists. The obtaining method gets a default body so `MockSystem` and any future backend need no change (the `set_bus_match` precedent). |
| 11 | 2026-07-23 | **`guidEventContext` echo filtering lives in the `win-audio` adapter; the port's contract is "events you did not cause".** | The GUID is a COM implementation detail. Leaking it upward would make every consumer responsible for a correctness rule the adapter can enforce once, and would put a Windows type in an engine-level contract. |
| 12 | 2026-07-23 | **Endpoint-driven gain changes persist as ordinary `ConfigEdit`s, exactly like a fader move.** | The binding is a control surface for the same value, so it should not produce a second class of state that behaves differently on restart. Writes are already debounced, so key-spam does not thrash the file. Rejected: live-only override (config.toml would only ever change from deliberate in-app edits, but the level would reset on restart and the displayed fader would disagree with the stored value). |
| 13 | 2026-07-23 | **At bind time, Windows wins: the endpoint's current level is adopted into the target's gain.** | The physical volume keys being the authority is the entire point of binding, and it makes the OSD and the app agree immediately. Accepted cost: engaging the binding overwrites the stored gain once. Rejected: pushing the stored gain out to the endpoint (preserves the user's fader, but changes a system-wide volume level as a startup side effect, which is a surprising thing for an app to do). |
| 14 | 2026-07-23 | **The port speaks position (0..1) mapped 1:1 onto fader travel, not dB.** | The dB APIs exist (`GetMasterVolumeLevel`, `GetVolumeRange`) but the notification payload carries only the scalar `fMasterVolume`, and this codebase has a logged reliability rule against calling synchronously back into the API that delivered a notification — so the callback must work from the payload alone. Position mapping also avoids guessing Windows' undocumented scalar-to-dB curve, and gives the user the property they actually perceive: the Windows slider and the Splitstream fader sit at the same place. |
| 15 | 2026-07-23 | **The push-to-mute state machine re-arms from actual state on a second `Pressed`, rather than trusting the remembered state.** | If a `Released` was missed, the remembered "prior state" is stale; restoring it on the next release would propagate the error instead of correcting it. Re-reading actual state on each press makes the machine self-healing rather than accumulating drift. |
| 16 | 2026-07-23 | **`spawn_hotkeys` takes one `&[HotkeyBinding]` list, superseding the profiles blueprint's `spawn_hotkeys(map, profiles, actions)`.** | Two features each adding their own parameter would give a third feature a third parameter. A binding is a chord plus an action, and every source of chords produces exactly that; collapsing to one list means new hotkey kinds are a new `HotkeyAction` variant rather than a new signature. Same collapse principle as replacing add/remove pairs with a whole-collection replace. Recorded as the shape whichever blueprint lands second should converge on. |
| 17 | 2026-07-23 | **Design approved at Level 4. Status set to `approved` -- ready for implementation.** | All four level sections persisted; no open questions. |

## Design: Level 2 -- Components

**Approved 2026-07-23.** `audio-core` is untouched and no new crates are added.

**Port shape follows existing precedent.** `ports/mod.rs` already has two
patterns: facade methods on `AudioSystem` for single-consumer concerns
(`subscribe_device_events`, a channel behind an `IMMNotificationClient`), and a
separate trait for "a distinct, best-effort, possibly-unavailable concern" with
its own owner (`SessionPort`, a documented exception to the grow-the-facade
rule). Endpoint volume is the second kind.

| # | Component | Layer | Change | Justification |
|---|---|---|---|---|
| 1 | `engine::ports::EndpointVolumePort` | orchestration (port) | **new** trait: `level`, `set_level`, `muted`, `set_muted`, `take_events` | Mirrors `SessionPort`'s rationale. A trait so the coordinator's logic is testable against a mock without COM — the reason `MockSystem` exists. |
| 2 | `AudioSystem::open_default_endpoint_volume()` | orchestration | **new** facade method returning `Box<dyn EndpointVolumePort>` | How you obtain one, mirroring `open_render`. Given a default body so `MockSystem` and future backends need no change — the `set_bus_match` precedent. |
| 3 | `VolumeEvent { level: f32, muted: bool }` | orchestration | **new** | The port emits only changes we did **not** cause; `guidEventContext` filtering lives in the adapter (decision 11). |
| 4 | `win_audio::endpoint_volume` | platform | **new** — `IAudioEndpointVolume` + `#[implement] IAudioEndpointVolumeCallback`, GUID echo filtering, `Drop`-time `UnregisterControlChangeNotify` | The register/unregister pair is the shape this codebase got wrong once (`WasapiSessions`) and right once (`DeviceMonitor`): explicit `Drop`, verified rather than assumed. |
| 5 | `engine::volume_bind` | orchestration | **new** module owning the port, re-subscribing on default-device change, surfacing events | Same shape as `routing.rs`'s coordinator + handle. |
| 6 | `reconcile(endpoint, target, suspended) -> Option<Action>` | orchestration | **new**, pure | The mirror rule including decision 4's guard, testable with no COM and no engine. |
| 7 | `step_gain(gain, delta_db) -> Gain` | shell | **new**, pure | Clamped 3 dB stepping, shared by every volume hotkey. |
| 8 | `ShellAction::{EndpointVolumeChanged, VolumeStep, ToggleGroupMute, PushToMute(bool)}` | shell | **new** variants | One action channel, as today. |
| 9 | `Dispatcher` binding arm | shell | applies endpoint changes as `ConfigEdit`s; pushes bound-target gain changes back out | Keeps the existing edit path as the single way state changes. |
| 10 | `hotkeys.rs` | shell | N chords, an action per binding, `Released` handling, max-hold timer | Consumes profiles' N-chord generalization; adds the hold state machine. |
| 11 | `tray.rs` | shell | `TrayCommand::Rebuild`, dynamic ids, per-group mute check items | Collides with profiles' tray changes — merge, do not apply literally. |
| 12 | config | control | `[app] volume_bind`; per-group `hotkey_mute`/`hotkey_volume_up`/`hotkey_volume_down`; `[hotkeys]` additions | Decision 8. |

**Components rejected:**

- **Folding endpoint volume onto `AudioSystem` as plain methods** — a separate
  concern with its own owner, per the `SessionPort` precedent.
- **A `VolumeBinding` domain type** — it is a name in config plus a suspended
  flag.
- **Exposing `guidEventContext` above the adapter** (decision 11).

## Design: Level 3 -- Interactions

**Approved 2026-07-23.** No domain events, no aggregate involvement —
`audio-core` never learns any of this exists.

**Position, not dB (decision 14).** `GetVolumeRange`/`GetMasterVolumeLevel`
exist in dB, but the notification payload carries only the scalar
(`fMasterVolume`), and this codebase has a logged rule against calling
synchronously back into the API that delivered a notification. So the port
speaks **position** (0..1), mapped 1:1 onto fader travel: the Windows slider and
the Splitstream fader show the same position. This avoids guessing Windows'
undocumented scalar→dB curve and avoids any API call inside `OnNotify`.

**Flow A — binding engages** (startup, or the user picks a target)

```
sys.open_default_endpoint_volume() -> port
suspended = target.output_device == default_output().name        // decision 4
if !suspended:
    ConfigEdit::SetGroupGain(target, from_position(port.level()))  // Windows wins, decision 13
  + ConfigEdit::SetMuted / SetGroupMute(port.muted())
```

**Flow B — Windows changes the volume** (keys, OSD, OS mixer)

```
IAudioEndpointVolumeCallback::OnNotify(data)
  -> if data.guidEventContext == OUR_GUID { return }     // adapter-side, decision 11
  -> channel.send(VolumeEvent { level: data.fMasterVolume, muted: data.bMuted })
     // payload only -- no API calls from inside the callback
volume_bind coordinator -> ShellAction::EndpointVolumeChanged
  -> Dispatcher -> ConfigEdit::SetGroupGain / SetMuted -> param fast path + debounced write
```

**Flow C — the Splitstream fader changes** -> after any edit touching the bound
target's gain or mute, `port.set_level(to_position(gain))` with **our** GUID.
The resulting notification carries that GUID and is dropped at flow B's first
line. No loop, and no invented echo suppression.

**Flow D — the guard** -> `suspended` is recomputed whenever the bound target's
`output_device` changes or the default device changes. On entering suspended:
stop mirroring both ways and surface the reason in the UI. On leaving:
re-adopt the endpoint level (flow A).

**Flow E — default device changes** -> `DeviceMonitor`'s existing
`IMMNotificationClient` fires -> drop the old port (its `Drop` unregisters),
open one on the new default, re-evaluate the guard, re-adopt.

**Flow F — volume hotkey** -> `ShellAction::VolumeStep { target, delta_db }` ->
`step_gain(current, ±3 dB)` clamped -> `SetGroupGain`/`SetMaster`. If the target
is the bound one, flow C pushes it outward so the OSD moves too.

**Flow G — push-to-mute**

```
Pressed  -> remember prior muted state -> set muted = true -> arm max-hold timer
Released -> restore the remembered state -> disarm timer
timer expires -> restore the remembered state (missed Released)   // capability 15
Pressed while already held -> re-arm from actual state, not remembered state
```

**Flow H — tray mute** -> menu event -> `ShellAction::ToggleGroupMute(name)` ->
the same `ConfigEdit` any other mute path uses. The check item's state refreshes
on the next tray rebuild.

**Flow I — failures** -> a device without volume control, a failed
`RegisterControlChangeNotify`, or a lost callback all leave the binding inert:
logged once, the fader keeps working, audio untouched. Matches the existing
best-effort convention for hotkeys and tray.

## Design: Level 4 -- Contracts

**Approved 2026-07-23.**

### `engine::ports`

```rust
/// A volume change we did **not** cause -- the adapter filters our own writes
/// by `guidEventContext` before emitting (decision 11).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeEvent {
    /// 0.0..=1.0 slider position, straight from the notification payload.
    pub level: f32,
    pub muted: bool,
}

/// Read/write access to one endpoint's master volume, plus notifications.
/// Separate from `AudioSystem` for `SessionPort`'s reason: a distinct,
/// best-effort, possibly-unavailable concern with its own owner.
pub trait EndpointVolumePort: Send {
    fn level(&self) -> Result<f32, PortError>;
    /// Tagged with this port's own GUID, so the resulting notification is
    /// filtered out and never re-enters as an event.
    fn set_level(&self, level: f32) -> Result<(), PortError>;
    fn muted(&self) -> Result<bool, PortError>;
    fn set_muted(&self, muted: bool) -> Result<(), PortError>;
    /// Single-consume, same pattern as `SessionPort::take_events`.
    fn take_events(&mut self) -> Receiver<VolumeEvent>;
}

// On AudioSystem -- default body so MockSystem and future backends need no change:
fn open_default_endpoint_volume(&self) -> Result<Box<dyn EndpointVolumePort>, PortError> {
    Err(PortError::Backend("endpoint volume not supported by this backend".into()))
}
```

### `engine::volume_bind`

```rust
pub fn start_volume_bind(sys: Arc<dyn AudioSystem>) -> VolumeBindHandle;

impl VolumeBindHandle {
    pub fn events(&self) -> &Receiver<VolumeEvent>;
    /// Best-effort outbound push (flow C). Silently inert when unbound,
    /// suspended, or the port failed to open.
    pub fn push_level(&self, level: f32);
    pub fn push_muted(&self, muted: bool);
    /// Re-open against the new default device (flow E).
    pub fn rebind(&self);
    pub fn set_suspended(&self, suspended: bool);
    pub fn shutdown(self);
}

/// Pure. `None` = do nothing: suspended, or already equal within
/// `MIRROR_EPSILON` -- without that tolerance the two sides ping-pong on float
/// rounding.
pub enum MirrorAction {
    AdoptFromEndpoint { level: f32, muted: bool },
    PushToEndpoint { level: f32 },
}
pub fn reconcile(
    endpoint: VolumeEvent,
    target_level: f32,
    target_muted: bool,
    suspended: bool,
) -> Option<MirrorAction>;
```

### `app`

```rust
pub enum VolumeTarget { Master, Group(String) }

pub enum ShellAction {
    // ...
    EndpointVolumeChanged(VolumeEvent),
    VolumeStep { target: VolumeTarget, delta_db: f32 },
    ToggleGroupMute(String),
    /// true = pressed, false = released or max-hold expired.
    PushToMute(bool),
}

/// Clamped to the fader's range. Pure.
fn step_gain(gain: Gain, delta_db: f32) -> Gain;

/// Pure push-to-mute state machine, so capabilities 14 and 15 are testable
/// without threads or a clock. `Some(muted)` = apply this mute state.
enum HoldEvent { Pressed { actual_muted: bool }, Released, Expired }
struct HoldState { held: bool, restore_to: bool }
fn push_to_mute(state: HoldState, event: HoldEvent) -> (HoldState, Option<bool>);

const VOLUME_STEP_DB: f32 = 3.0;
const PUSH_TO_MUTE_MAX_HOLD: Duration = Duration::from_secs(30);
const MIRROR_EPSILON: f32 = 0.005;
```

### `hotkeys.rs` -- supersedes the profiles blueprint's signature

```rust
pub enum HotkeyAction {
    ToggleMasterMute,
    PushToMuteMaster,
    VolumeUp(VolumeTarget),
    VolumeDown(VolumeTarget),
    ToggleGroupMute(String),
    ApplyProfile(String),          // from the profiles blueprint
}
pub struct HotkeyBinding { pub chord: HotkeyChord, pub action: HotkeyAction }

pub fn spawn_hotkeys(
    bindings: &[HotkeyBinding],
    actions: Sender<ShellAction>,
) -> Result<HotkeyHandle, ShellError>;
```

Profiles' version was `spawn_hotkeys(map, profiles, actions)`. One binding list
beats two parallel parameters -- whichever lands second should collapse to this
shape rather than adding a third parameter (decision 16).

### `tray.rs` -- also a merge point with profiles

```rust
pub struct TrayGroup { pub name: String, pub muted: bool }
pub struct TrayModel {
    pub groups: Vec<TrayGroup>,
    pub profiles: Vec<String>,          // from the profiles blueprint
    pub active_profile: Option<String>,
    pub master_muted: bool,
}
pub enum TrayCommand { Quit, Notice(String), Rebuild(TrayModel) }
```

### Config

```toml
[app]
volume_bind = "Game"        # or "master", or absent

[hotkeys]
mute_master        = "Ctrl+Alt+M"
push_to_mute       = "Ctrl+Alt+Space"
master_volume_up   = "Ctrl+Alt+Up"
master_volume_down = "Ctrl+Alt+Down"

[[group]]
name = "Game"
hotkey_mute        = "Ctrl+Alt+1"
hotkey_volume_up   = "Ctrl+Shift+Up"
hotkey_volume_down = "Ctrl+Shift+Down"
```

### Test contracts

| Layer | Test |
|---|---|
| `engine::volume_bind` | `a_suspended_binding_mirrors_nothing` |
| `engine::volume_bind` | `equal_within_epsilon_produces_no_action` -- the ping-pong guard |
| `engine::volume_bind` | `an_endpoint_change_is_adopted` |
| `engine::volume_bind` | `a_target_change_is_pushed_outward` |
| `engine::volume_bind` | `a_mute_change_is_adopted` |
| `app` | `step_gain_clamps_at_the_top` / `..._at_the_bottom` |
| `app` | `push_to_mute_restores_the_prior_muted_state` -- capability 14 |
| `app` | `push_to_mute_restores_on_expiry_when_release_is_missed` -- capability 15 |
| `app` | `a_second_press_re_arms_from_actual_state` -- decision 15 |
| `control` | `volume_bind_and_per_group_hotkeys_round_trip_through_toml` |
| `control` | `an_absent_hotkey_registers_nothing` |

## Open Questions

*(none -- every judgment call raised during design was resolved and logged)*

## Design Summary

**Drift check: skipped -- `requirement_doc` is null.** Origin is
`.lattice/ux-gap-roadmap.md` Priorities 9 and 10 plus a user-raised idea, not a
requirement spec, so there are no Scenarios/ACs or `## Technical Constraints` to
compare Level 4 against and nothing to write back.

**Components and layers**

| Layer | Components |
|---|---|
| domain (`audio-core`) | **untouched** |
| orchestration (`engine`) | `EndpointVolumePort`, `VolumeEvent`, `AudioSystem::open_default_endpoint_volume`, `engine::volume_bind` (handle + pure `reconcile`) |
| platform (`win-audio`) | `endpoint_volume`: `IAudioEndpointVolume`, `#[implement] IAudioEndpointVolumeCallback`, GUID echo filtering, `Drop`-time unregister |
| shell (`app`) | `VolumeTarget`, four `ShellAction` variants, dispatcher binding arm, pure `step_gain` and `push_to_mute`, `hotkeys.rs` binding list, `tray.rs` rebuild model |
| control | `[app] volume_bind`, per-group hotkey fields, `[hotkeys]` additions |

**Key contracts** -- `EndpointVolumePort` (whose contract is "events you did not
cause"), the pure `reconcile`, and the pure `push_to_mute` state machine. The
three pure functions carry every behaviour that is easy to get wrong.

**Architectural constraints honored**

- The port boundary keeps COM out of `engine`: no GUID, no `HRESULT`, no
  `windows` type crosses it.
- The callback does no work and calls nothing back into the API -- it reads the
  payload and posts to a channel, per the logged notification-callback rule.
- `RegisterControlChangeNotify` gets an explicit `Drop`-time
  `UnregisterControlChangeNotify`, the pair this codebase has already got wrong
  once.
- Every new surface is best-effort: failure logs and leaves audio untouched.
- All state changes still flow through the existing `ConfigEdit` path, so there
  is no second way for gain or mute to change.

**Domain model** -- nothing domain-side. `VolumeEvent` is a data carrier,
`MirrorAction` and `HoldState` are transient control state.

**Open questions resolved during design** -- whether binding replaces tray
volume (decision 1), which endpoint to follow (2), sync direction (3),
persistence (12), startup authority (13), and the unit crossing the port (14).

**Merge points recorded** -- `spawn_hotkeys` and `tray.rs` are both restructured
by the profiles blueprint as well; the Level 4 shapes here are the collapsed
versions both should converge on.
