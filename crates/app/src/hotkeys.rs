//! Global hotkeys: config-defined chords → `ShellAction`. Runs its own `tao`
//! event loop on a dedicated background thread — same event-loop-affinity
//! requirement as tray.rs (`global-hotkey` needs an active native event loop
//! on its creation thread, verified against docs.rs). No-ops (returns an
//! idle handle, spawns nothing) when the config defines no hotkeys.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::EventLoopBuilderExtWindows;

use engine::HotkeyChord;

use crate::lifecycle::ShellError;
use crate::{ShellAction, VolumeTarget, VOLUME_STEP_DB};

enum HotkeyCommand {
    Quit,
}

/// What a registered chord does once pressed (external-controls.md decision
/// 16 — supersedes profiles.md's `spawn_hotkeys(map, profiles, actions)`
/// with one binding list, so a new hotkey kind is a new variant here rather
/// than a new function parameter).
#[derive(Clone)]
pub enum HotkeyAction {
    ToggleMasterMute,
    /// Held while pressed, restores the prior state on release or max-hold
    /// expiry (capabilities 13-15) — the only action that reacts to
    /// `HotKeyState::Released` at all; every other binding ignores it.
    PushToMuteMaster,
    VolumeUp(VolumeTarget),
    VolumeDown(VolumeTarget),
    ToggleGroupMute(String),
    ApplyProfile(String),
}

/// A chord paired with what it does — the whole hotkey config surface
/// (master, per-group, push-to-mute, profiles) reduces to one `Vec` of these.
pub struct HotkeyBinding {
    pub chord: HotkeyChord,
    pub action: HotkeyAction,
}

pub struct HotkeyHandle {
    proxy: Option<EventLoopProxy<HotkeyCommand>>,
    thread: Option<JoinHandle<()>>,
}

impl HotkeyHandle {
    /// No hotkeys registered / registration failed — nothing to shut down.
    pub fn idle() -> HotkeyHandle {
        HotkeyHandle { proxy: None, thread: None }
    }

    pub fn shutdown(self) {
        if let Some(proxy) = self.proxy {
            let _ = proxy.send_event(HotkeyCommand::Quit);
        }
        if let Some(thread) = self.thread {
            let _ = thread.join();
        }
    }
}

/// Registers every binding whose chord converts successfully. No-ops when
/// `bindings` is empty. A chord that fails to convert or register is
/// skipped individually — a bad binding never blocks the others or audio
/// itself.
pub fn spawn_hotkeys(bindings: &[HotkeyBinding], actions: Sender<ShellAction>) -> Result<HotkeyHandle, ShellError> {
    let mut converted: Vec<(HotKey, HotkeyAction)> = Vec::new();
    for binding in bindings {
        if let Ok(hotkey) = to_global_hotkey(binding.chord) {
            converted.push((hotkey, binding.action.clone()));
        }
    }
    if converted.is_empty() {
        return Ok(HotkeyHandle::idle());
    }

    let (proxy_tx, proxy_rx) = std::sync::mpsc::channel();
    let thread = thread::spawn(move || {
        let mut event_loop = EventLoopBuilder::<HotkeyCommand>::with_user_event()
            .with_any_thread(true)
            .build();
        if proxy_tx.send(event_loop.create_proxy()).is_err() {
            return; // caller gave up waiting
        }
        run_hotkeys(&mut event_loop, converted, actions);
    });

    let proxy = proxy_rx
        .recv()
        .map_err(|_| ShellError::Hotkey("hotkey thread died before starting".into()))?;
    Ok(HotkeyHandle {
        proxy: Some(proxy),
        thread: Some(thread),
    })
}

fn run_hotkeys(
    event_loop: &mut EventLoop<HotkeyCommand>,
    bindings: Vec<(HotKey, HotkeyAction)>,
    actions: Sender<ShellAction>,
) {
    // Manager must be created on this thread — same event-loop-affinity
    // requirement as tray-icon. §9.3-style best-effort: a registration
    // failure just means that one binding never fires, audio is unaffected.
    let Ok(manager) = GlobalHotKeyManager::new() else { return };
    let by_id: HashMap<u32, &HotkeyAction> = bindings
        .iter()
        .filter(|(hotkey, _)| manager.register(*hotkey).is_ok())
        .map(|(hotkey, action)| (hotkey.id(), action))
        .collect();

    let hotkey_events = GlobalHotKeyEvent::receiver();

    event_loop.run_return(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;

        while let Ok(evt) = hotkey_events.try_recv() {
            if let Some(action) = by_id.get(&evt.id) {
                if let Some(shell_action) = to_shell_action(action, evt.state) {
                    let _ = actions.send(shell_action);
                }
            }
        }

        if let Event::UserEvent(HotkeyCommand::Quit) = event {
            *control_flow = ControlFlow::Exit;
        }
    });
}

/// `None` = this (action, state) pair fires nothing — every binding but
/// `PushToMuteMaster` ignores `Released` (the mechanism already exists in
/// `global-hotkey`/Windows; earlier code just discarded that half of it).
fn to_shell_action(action: &HotkeyAction, state: HotKeyState) -> Option<ShellAction> {
    match (action, state) {
        (HotkeyAction::PushToMuteMaster, HotKeyState::Pressed) => Some(ShellAction::PushToMute(true)),
        (HotkeyAction::PushToMuteMaster, HotKeyState::Released) => Some(ShellAction::PushToMute(false)),
        (_, HotKeyState::Released) => None,
        (HotkeyAction::ToggleMasterMute, HotKeyState::Pressed) => Some(ShellAction::ToggleMute),
        (HotkeyAction::ToggleGroupMute(name), HotKeyState::Pressed) => {
            Some(ShellAction::ToggleGroupMute(name.clone()))
        }
        (HotkeyAction::ApplyProfile(name), HotKeyState::Pressed) => Some(ShellAction::ApplyProfile(name.clone())),
        (HotkeyAction::VolumeUp(target), HotKeyState::Pressed) => {
            Some(ShellAction::VolumeStep { target: target.clone(), delta_db: VOLUME_STEP_DB })
        }
        (HotkeyAction::VolumeDown(target), HotKeyState::Pressed) => {
            Some(ShellAction::VolumeStep { target: target.clone(), delta_db: -VOLUME_STEP_DB })
        }
    }
}

fn to_global_hotkey(chord: HotkeyChord) -> Result<HotKey, ShellError> {
    let mut mods = Modifiers::empty();
    if chord.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if chord.alt {
        mods |= Modifiers::ALT;
    }
    if chord.shift {
        mods |= Modifiers::SHIFT;
    }
    let code = key_code(chord.key)
        .ok_or_else(|| ShellError::Hotkey(format!("unsupported hotkey key {:?}", chord.key)))?;
    Ok(HotKey::new(Some(mods), code))
}

fn key_code(key: engine::HotkeyKey) -> Option<Code> {
    let c = match key {
        engine::HotkeyKey::Space => return Some(Code::Space),
        engine::HotkeyKey::Up => return Some(Code::ArrowUp),
        engine::HotkeyKey::Down => return Some(Code::ArrowDown),
        engine::HotkeyKey::Char(c) => c,
    };
    Some(match c {
        'A' => Code::KeyA,
        'B' => Code::KeyB,
        'C' => Code::KeyC,
        'D' => Code::KeyD,
        'E' => Code::KeyE,
        'F' => Code::KeyF,
        'G' => Code::KeyG,
        'H' => Code::KeyH,
        'I' => Code::KeyI,
        'J' => Code::KeyJ,
        'K' => Code::KeyK,
        'L' => Code::KeyL,
        'M' => Code::KeyM,
        'N' => Code::KeyN,
        'O' => Code::KeyO,
        'P' => Code::KeyP,
        'Q' => Code::KeyQ,
        'R' => Code::KeyR,
        'S' => Code::KeyS,
        'T' => Code::KeyT,
        'U' => Code::KeyU,
        'V' => Code::KeyV,
        'W' => Code::KeyW,
        'X' => Code::KeyX,
        'Y' => Code::KeyY,
        'Z' => Code::KeyZ,
        '0' => Code::Digit0,
        '1' => Code::Digit1,
        '2' => Code::Digit2,
        '3' => Code::Digit3,
        '4' => Code::Digit4,
        '5' => Code::Digit5,
        '6' => Code::Digit6,
        '7' => Code::Digit7,
        '8' => Code::Digit8,
        '9' => Code::Digit9,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_binding_but_push_to_mute_ignores_released() {
        for action in [
            HotkeyAction::ToggleMasterMute,
            HotkeyAction::ToggleGroupMute("Game".into()),
            HotkeyAction::ApplyProfile("Gaming".into()),
            HotkeyAction::VolumeUp(VolumeTarget::Master),
            HotkeyAction::VolumeDown(VolumeTarget::Group("Game".into())),
        ] {
            assert!(to_shell_action(&action, HotKeyState::Released).is_none());
        }
    }

    #[test]
    fn push_to_mute_fires_on_both_pressed_and_released() {
        assert!(matches!(
            to_shell_action(&HotkeyAction::PushToMuteMaster, HotKeyState::Pressed),
            Some(ShellAction::PushToMute(true))
        ));
        assert!(matches!(
            to_shell_action(&HotkeyAction::PushToMuteMaster, HotKeyState::Released),
            Some(ShellAction::PushToMute(false))
        ));
    }

    #[test]
    fn volume_up_and_down_step_in_opposite_directions() {
        let up = to_shell_action(&HotkeyAction::VolumeUp(VolumeTarget::Master), HotKeyState::Pressed);
        let down = to_shell_action(&HotkeyAction::VolumeDown(VolumeTarget::Master), HotKeyState::Pressed);
        match (up, down) {
            (
                Some(ShellAction::VolumeStep { delta_db: up_db, .. }),
                Some(ShellAction::VolumeStep { delta_db: down_db, .. }),
            ) => {
                assert_eq!(up_db, VOLUME_STEP_DB);
                assert_eq!(down_db, -VOLUME_STEP_DB);
            }
            other => panic!("expected two VolumeStep actions, got {other:?}"),
        }
    }

    #[test]
    fn toggle_group_mute_and_apply_profile_carry_the_right_name() {
        assert!(matches!(
            to_shell_action(&HotkeyAction::ToggleGroupMute("Game".into()), HotKeyState::Pressed),
            Some(ShellAction::ToggleGroupMute(name)) if name == "Game"
        ));
        assert!(matches!(
            to_shell_action(&HotkeyAction::ApplyProfile("Gaming".into()), HotKeyState::Pressed),
            Some(ShellAction::ApplyProfile(name)) if name == "Gaming"
        ));
    }
}
