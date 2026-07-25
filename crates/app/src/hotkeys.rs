//! Global hotkeys: config-defined chords → `ShellAction`. Runs its own `tao`
//! event loop on a dedicated background thread — same event-loop-affinity
//! requirement as tray.rs (`global-hotkey` needs an active native event loop
//! on its creation thread, verified against docs.rs). No-ops (returns an
//! idle handle, spawns nothing) when the config defines no hotkeys.

use std::sync::mpsc::Sender;
use std::thread::{self, JoinHandle};

use std::collections::HashMap;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::EventLoopBuilderExtWindows;

use engine::{HotkeyChord, HotkeyMap, ProfileConfig};

use crate::lifecycle::ShellError;
use crate::ShellAction;

enum HotkeyCommand {
    Quit,
}

/// What a registered chord does once pressed (profiles.md decision 7 —
/// generalizes `spawn_hotkeys` from exactly one registered chord to N).
enum HotkeyAction {
    ToggleMute,
    ApplyProfile(String),
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

/// Registers `map.mute_master` (if set) plus one chord per profile that
/// names a `hotkey` (profiles.md capability 10/decision 7). No-ops when
/// neither exists. A chord that fails to parse or register is skipped
/// individually — same best-effort spirit as the single-hotkey case: a bad
/// binding never blocks the others or audio itself.
pub fn spawn_hotkeys(
    map: &HotkeyMap,
    profiles: &[ProfileConfig],
    actions: Sender<ShellAction>,
) -> Result<HotkeyHandle, ShellError> {
    let mut bindings: Vec<(HotKey, HotkeyAction)> = Vec::new();
    if let Some(chord) = map.mute_master {
        if let Ok(hotkey) = to_global_hotkey(chord) {
            bindings.push((hotkey, HotkeyAction::ToggleMute));
        }
    }
    for profile in profiles {
        if let Some(chord) = profile.hotkey {
            if let Ok(hotkey) = to_global_hotkey(chord) {
                bindings.push((hotkey, HotkeyAction::ApplyProfile(profile.name.clone())));
            }
        }
    }
    if bindings.is_empty() {
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
        run_hotkeys(&mut event_loop, bindings, actions);
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
            if evt.state != HotKeyState::Pressed {
                continue;
            }
            if let Some(action) = by_id.get(&evt.id) {
                let shell_action = match action {
                    HotkeyAction::ToggleMute => ShellAction::ToggleMute,
                    HotkeyAction::ApplyProfile(name) => ShellAction::ApplyProfile(name.clone()),
                };
                let _ = actions.send(shell_action);
            }
        }

        if let Event::UserEvent(HotkeyCommand::Quit) = event {
            *control_flow = ControlFlow::Exit;
        }
    });
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

fn key_code(key: char) -> Option<Code> {
    Some(match key {
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
