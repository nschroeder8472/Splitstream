//! Tray icon: quick menu (mute, settings, quit), tooltip surfaces the latest
//! engine notice. Runs its own `tao` event loop on a dedicated background
//! thread — `tray-icon`/`muda` require an active native event loop on their
//! creation thread (verified against docs.rs), but it doesn't need to be the
//! app's main thread. Decoupled from the eframe/settings-window lifecycle on
//! purpose: the icon must stay resident whether or not the settings window
//! is open (app-shell.md L1 §1 vs §2 are independent capabilities).

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};

use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::windows::EventLoopBuilderExtWindows;
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, Submenu};
use tray_icon::{Icon, TrayIconBuilder};

use engine::{EngineEvent, ProfileConfig};

use crate::ShellAction;

enum TrayCommand {
    Quit,
    Notice(String),
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
}

pub fn spawn_tray(
    actions: Sender<ShellAction>,
    notices: Receiver<EngineEvent>,
    profiles: Vec<ProfileConfig>,
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
        run_tray(&mut event_loop, actions, profiles);
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
                if proxy.send_event(TrayCommand::Notice(describe_notice(&evt))).is_err() {
                    return; // tray thread gone
                }
            }
        });
    }

    TrayHandle { proxy, thread }
}

fn describe_notice(evt: &EngineEvent) -> String {
    match evt {
        EngineEvent::FallbackApplied { to, .. } => format!("Splitstream — fell back to {}", to.0),
        EngineEvent::Recovered { on, .. } => format!("Splitstream — recovered on {}", on.0),
        EngineEvent::DeviceAvailable(ep) => format!("Splitstream — device available: {}", ep.name),
        EngineEvent::DeviceLost { .. } => "Splitstream — output device lost".to_string(),
        EngineEvent::RoutingDegraded { reason } => format!("Splitstream — routing degraded: {reason}"),
    }
}

struct MenuIds {
    mute: MenuId,
    settings: MenuId,
    quit: MenuId,
    /// Profile submenu items (profiles.md capability 9) — id to profile
    /// name, empty when the config defines no profiles (no submenu built
    /// at all in that case, same "purely additive" behavior as elsewhere).
    profiles: HashMap<MenuId, String>,
}

fn build_menu(profiles: &[ProfileConfig]) -> (Menu, MenuIds) {
    let mute_item = MenuItem::new("Mute", true, None);
    let settings_item = MenuItem::new("Settings", true, None);
    let quit_item = MenuItem::new("Quit", true, None);

    let mut ids = MenuIds {
        mute: mute_item.id().clone(),
        settings: settings_item.id().clone(),
        quit: quit_item.id().clone(),
        profiles: HashMap::new(),
    };

    let menu = Menu::new();
    let _ = menu.append(&mute_item);
    if !profiles.is_empty() {
        let submenu = Submenu::new("Profiles", true);
        for profile in profiles {
            let item = MenuItem::new(&profile.name, true, None);
            ids.profiles.insert(item.id().clone(), profile.name.clone());
            let _ = submenu.append(&item);
        }
        let _ = menu.append(&submenu);
    }
    let _ = menu.append(&settings_item);
    let _ = menu.append(&quit_item);
    (menu, ids)
}

fn run_tray(event_loop: &mut EventLoop<TrayCommand>, actions: Sender<ShellAction>, profiles: Vec<ProfileConfig>) {
    let (menu, ids) = build_menu(&profiles);

    let mut tray_icon = Some(
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Splitstream")
            .with_icon(placeholder_icon())
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
