//! Tray icon: quick menu (mute, per-group mute, profiles, settings, quit),
//! tooltip surfaces the latest engine notice. Runs its own `tao` event loop
//! on a dedicated background thread — `tray-icon`/`muda` require an active
//! native event loop on their creation thread (verified against docs.rs),
//! but it doesn't need to be the app's main thread. Decoupled from the
//! eframe/settings-window lifecycle on purpose: the icon must stay resident
//! whether or not the settings window is open (app-shell.md L1 §1 vs §2 are
//! independent capabilities).
//!
//! The menu rebuilds on demand (external-controls.md capability 9 — the
//! group set, not just profiles, must never go stale) rather than once at
//! startup like the profiles-only version this supersedes.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};

use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::EventLoopBuilderExtWindows;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder};

use engine::EngineEvent;

use crate::ShellAction;

/// One group's tray-relevant state — a name and whether it's muted.
#[derive(PartialEq)]
pub struct TrayGroup {
    pub name: String,
    pub muted: bool,
}

/// Everything the tray menu needs to draw itself, computed fresh by the
/// caller from the current snapshot on every relevant change. `PartialEq` so
/// the caller can skip `TrayHandle::rebuild` when nothing tray-relevant
/// actually changed — a native menu rebuild is real OS-level work, unlike
/// the plain-data comparison that guards it.
#[derive(PartialEq)]
pub struct TrayModel {
    pub groups: Vec<TrayGroup>,
    pub profiles: Vec<String>,
    pub active_profile: Option<String>,
    pub master_muted: bool,
}

enum TrayCommand {
    Quit,
    Notice(String),
    Rebuild(TrayModel),
    /// A freshly-rendered brand mark (visual-identity.md decision 9/Flow G) —
    /// `size` x `size` straight-alpha RGBA8, same shape `theme::brand_icon_rgba`
    /// returns. Separate from `Rebuild`: the icon and the menu change on
    /// different triggers (accent/system-theme vs. group/profile/mute state).
    SetIcon(Vec<u8>, u32),
}

pub struct TrayHandle {
    proxy: EventLoopProxy<TrayCommand>,
    thread: JoinHandle<()>,
}

impl TrayHandle {
    pub fn shutdown(self) {
        let _ = self.proxy.send_event(TrayCommand::Quit);
        let _ = self.thread.join();
    }

    /// Rebuilds the menu from a fresh model (capability 9). Best-effort,
    /// same as every other cross-thread tray command — a dropped tray
    /// thread just means the next rebuild is also silently skipped, not a
    /// crash.
    pub fn rebuild(&self, model: TrayModel) {
        let _ = self.proxy.send_event(TrayCommand::Rebuild(model));
    }

    /// Pushes a freshly-rendered brand mark (decision 9/Flow G). Best-effort,
    /// same as [`Self::rebuild`].
    pub fn set_icon(&self, rgba: Vec<u8>, size: u32) {
        let _ = self.proxy.send_event(TrayCommand::SetIcon(rgba, size));
    }
}

pub fn spawn_tray(
    actions: Sender<ShellAction>,
    notices: Receiver<EngineEvent>,
    initial: TrayModel,
    initial_icon_rgba: Vec<u8>,
    initial_icon_size: u32,
) -> TrayHandle {
    // The `EventLoop` ties itself to the OS message queue of whichever
    // thread builds it, so it's built *inside* the spawned thread, not moved
    // in from outside. Its `EventLoopProxy` (Send + Clone, made for exactly
    // this) is handed back out over a one-shot channel.
    let (proxy_tx, proxy_rx) = std::sync::mpsc::channel();

    let thread = thread::spawn(move || {
        let mut event_loop = EventLoopBuilder::<TrayCommand>::with_user_event()
            .with_any_thread(true)
            .build();
        if proxy_tx.send(event_loop.create_proxy()).is_err() {
            return; // caller gave up waiting
        }
        run_tray(&mut event_loop, actions, initial, initial_icon_rgba, initial_icon_size);
    });

    let proxy = proxy_rx
        .recv()
        .expect("tray thread died before creating its event loop");

    // `notices.recv()` blocks, so it can't share the tray's own event-loop
    // thread — a second thread relays each notice in as a `TrayCommand`.
    {
        let proxy = proxy.clone();
        thread::spawn(move || {
            for evt in notices {
                if evt_is_silent(&evt) {
                    continue;
                }
                if proxy.send_event(TrayCommand::Notice(describe_notice(&evt))).is_err() {
                    return; // tray thread gone
                }
            }
        });
    }

    TrayHandle { proxy, thread }
}

/// `DefaultDeviceChanged` is internal plumbing (external-controls.md flow E)
/// consumed by the dispatcher, not a user-facing notice — filtered here so
/// it never reaches `describe_notice`/the tooltip.
/// `DeviceRemoved` is silent for the same reason plus one of its own: it
/// carries only a raw endpoint id, never a friendly name, and the removals a
/// user cares about already arrive as `DeviceLost`/`FallbackApplied` with the
/// audio consequence attached. A vanished *sink* is explained by the settings
/// window's own banner, where there is room to say what it means.
fn evt_is_silent(evt: &EngineEvent) -> bool {
    matches!(
        evt,
        EngineEvent::DefaultDeviceChanged(_) | EngineEvent::DeviceRemoved(_)
    )
}

fn describe_notice(evt: &EngineEvent) -> String {
    match evt {
        EngineEvent::FallbackApplied { to, .. } => format!("Splitstream — fell back to {}", to.0),
        EngineEvent::Recovered { on, .. } => format!("Splitstream — recovered on {}", on.0),
        EngineEvent::DeviceAvailable(ep) => format!("Splitstream — device available: {}", ep.name),
        EngineEvent::DeviceLost { .. } => "Splitstream — output device lost".to_string(),
        EngineEvent::RoutingDegraded { reason } => format!("Splitstream — routing degraded: {reason}"),
        // Both filtered by `evt_is_silent` above; kept for exhaustiveness.
        EngineEvent::DefaultDeviceChanged(_) | EngineEvent::DeviceRemoved(_) => String::new(),
    }
}

struct MenuIds {
    mute: MenuId,
    settings: MenuId,
    quit: MenuId,
    /// Per-group mute check items (capability 8) — id to group name.
    group_mutes: HashMap<MenuId, String>,
    /// Profile submenu items (profiles.md capability 9) — id to profile name.
    profiles: HashMap<MenuId, String>,
}

fn build_menu(model: &TrayModel) -> (Menu, MenuIds) {
    let mute_item = CheckMenuItem::new("Mute", true, model.master_muted, None);
    let settings_item = MenuItem::new("Settings", true, None);
    let quit_item = MenuItem::new("Quit", true, None);

    let mut ids = MenuIds {
        mute: mute_item.id().clone(),
        settings: settings_item.id().clone(),
        quit: quit_item.id().clone(),
        group_mutes: HashMap::new(),
        profiles: HashMap::new(),
    };

    let menu = Menu::new();
    let _ = menu.append(&mute_item);

    if !model.groups.is_empty() {
        let submenu = Submenu::new("Groups", true);
        for group in &model.groups {
            let item = CheckMenuItem::new(&group.name, true, group.muted, None);
            ids.group_mutes.insert(item.id().clone(), group.name.clone());
            let _ = submenu.append(&item);
        }
        let _ = menu.append(&submenu);
    }

    if !model.profiles.is_empty() {
        let submenu = Submenu::new("Profiles", true);
        for name in &model.profiles {
            let is_active = model.active_profile.as_deref() == Some(name.as_str());
            let item = CheckMenuItem::new(name, true, is_active, None);
            ids.profiles.insert(item.id().clone(), name.clone());
            let _ = submenu.append(&item);
        }
        let _ = menu.append(&submenu);
    }

    let _ = menu.append(&settings_item);
    let _ = menu.append(&quit_item);
    (menu, ids)
}

fn run_tray(
    event_loop: &mut EventLoop<TrayCommand>,
    actions: Sender<ShellAction>,
    initial: TrayModel,
    initial_icon_rgba: Vec<u8>,
    initial_icon_size: u32,
) {
    let (menu, mut ids) = build_menu(&initial);

    let initial_icon = Icon::from_rgba(initial_icon_rgba, initial_icon_size, initial_icon_size)
        .unwrap_or_else(|_| placeholder_icon());
    let mut tray_icon = Some(
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Splitstream")
            .with_icon(initial_icon)
            .build()
            .expect("tray icon creation failed"),
    );

    let menu_events = MenuEvent::receiver();

    event_loop.run_return(move |event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;

        while let Ok(menu_event) = menu_events.try_recv() {
            if menu_event.id == ids.mute {
                let _ = actions.send(ShellAction::ToggleMute);
            } else if menu_event.id == ids.settings {
                let _ = actions.send(ShellAction::ShowSettings);
            } else if menu_event.id == ids.quit {
                let _ = actions.send(ShellAction::Quit);
                *control_flow = ControlFlow::Exit;
            } else if let Some(name) = ids.profiles.get(&menu_event.id) {
                let _ = actions.send(ShellAction::ApplyProfile(name.clone()));
            } else if let Some(name) = ids.group_mutes.get(&menu_event.id) {
                let _ = actions.send(ShellAction::ToggleGroupMute(name.clone()));
            }
        }

        if let Event::UserEvent(cmd) = event {
            match cmd {
                TrayCommand::Quit => *control_flow = ControlFlow::Exit,
                TrayCommand::Notice(text) => {
                    if let Some(icon) = &tray_icon {
                        let _ = icon.set_tooltip(Some(text));
                    }
                }
                TrayCommand::Rebuild(model) => {
                    let (menu, new_ids) = build_menu(&model);
                    if let Some(icon) = &tray_icon {
                        icon.set_menu(Some(Box::new(menu)));
                    }
                    ids = new_ids;
                }
                TrayCommand::SetIcon(rgba, size) => {
                    if let Some(icon) = &tray_icon {
                        if let Ok(ic) = Icon::from_rgba(rgba, size, size) {
                            let _ = icon.set_icon(Some(ic));
                        }
                    }
                }
            }
        }

        if *control_flow == ControlFlow::Exit {
            tray_icon.take(); // drop the icon before the loop actually stops
        }
    });
}

/// A fixed solid-color square — the real branded asset is a packaging
/// concern (N6), not designed here.
fn placeholder_icon() -> Icon {
    const SIZE: u32 = 16;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for _ in 0..(SIZE * SIZE) {
        rgba.extend_from_slice(&[0x30, 0x9c, 0xff, 0xff]);
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("fixed-size RGBA buffer is always a valid icon")
}
