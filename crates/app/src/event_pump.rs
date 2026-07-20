//! Fans the engine's single-consume `EngineEvent` receiver out to the tray
//! (for notifications) and nudges `UiState.routing_degraded` on a
//! degradation notice.
//!
//! Deliberately does **not** own a `RoutingHandle`, unlike app-shell.md's
//! design-revision sketch ("update_topology call to routing after
//! structural-rebuild events"): `RoutingHandle` can't be `Clone` (it owns the
//! coordinator thread's `JoinHandle`), and no `EngineEvent` variant signals
//! "routes changed" or "structural rebuild done" — most route changes
//! (session add/remove, rule edits) never touch this channel at all. Instead:
//! `update_rules`/`update_topology` stay with the dispatcher, called at the
//! exact synchronous points flows C/D/H already specify, and the settings
//! window polls `engine::RoutingReader` every frame to keep `routes`/
//! `routing_degraded` fresh — a poll is strictly more correct here than
//! inferring a route change from unrelated event arrival.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use audio_core::GroupId;
use engine::{ConfigSnapshot, EngineEvent, EngineStats, SessionInfo};

/// Shared read model for the settings window and tray — the app-shell.md L4
/// `UiState` contract. `app`-internal; nothing in `engine`/`control` reads it.
pub struct UiState {
    pub snapshot: ConfigSnapshot,
    pub routes: Vec<(GroupId, Vec<SessionInfo>)>,
    pub stats: EngineStats,
    pub routing_degraded: bool,
}

pub struct PumpHandle {
    thread: JoinHandle<()>,
}

impl PumpHandle {
    /// The pump thread exits on its own once `events`'s sender side drops
    /// (`EngineHandle::shutdown`) — this just waits for that to happen.
    pub fn shutdown(self) {
        let _ = self.thread.join();
    }
}

pub struct EventPump;

impl EventPump {
    pub fn spawn(events: Receiver<EngineEvent>, tray: Sender<EngineEvent>, ui: Arc<Mutex<UiState>>) -> PumpHandle {
        let thread = thread::spawn(move || {
            for evt in events {
                if matches!(evt, EngineEvent::RoutingDegraded { .. }) {
                    ui.lock().unwrap().routing_degraded = true;
                }
                // Tray gone (shutting down): keep draining anyway so a
                // send on `events`'s far side never blocks on a dead pump.
                let _ = tray.send(evt);
            }
        });
        PumpHandle { thread }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::ConfigSnapshot;
    use std::sync::mpsc;
    use std::time::Duration;

    fn empty_ui() -> Arc<Mutex<UiState>> {
        Arc::new(Mutex::new(UiState {
            snapshot: test_snapshot(),
            routes: vec![],
            stats: EngineStats {
                xruns: 0,
                ring_fill: vec![],
                applied_ratio: vec![],
                group_faults: vec![],
            },
            routing_degraded: false,
        }))
    }

    fn test_snapshot() -> ConfigSnapshot {
        ConfigSnapshot {
            schema_version: 2,
            master: audio_core::Gain::UNITY,
            muted: false,
            groups: vec![],
            app: engine::AppConfig::default(),
        }
    }

    #[test]
    fn forwards_every_event_to_the_tray_channel() {
        let (events_tx, events_rx) = mpsc::channel();
        let (tray_tx, tray_rx) = mpsc::channel();
        let ui = empty_ui();

        let pump = EventPump::spawn(events_rx, tray_tx, ui);
        events_tx
            .send(EngineEvent::DeviceLost { groups: vec![GroupId(0)] })
            .unwrap();
        drop(events_tx);

        let received = tray_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(received, EngineEvent::DeviceLost { .. }));
        pump.shutdown();
    }

    #[test]
    fn routing_degraded_event_sets_the_ui_flag() {
        let (events_tx, events_rx) = mpsc::channel();
        let (tray_tx, _tray_rx) = mpsc::channel();
        let ui = empty_ui();

        let pump = EventPump::spawn(events_rx, tray_tx, Arc::clone(&ui));
        events_tx
            .send(EngineEvent::RoutingDegraded { reason: "test".into() })
            .unwrap();
        drop(events_tx);
        pump.shutdown();

        assert!(ui.lock().unwrap().routing_degraded);
    }

    #[test]
    fn pump_thread_exits_once_the_events_sender_drops() {
        let (events_tx, events_rx) = mpsc::channel();
        let (tray_tx, _tray_rx) = mpsc::channel();
        let ui = empty_ui();

        let pump = EventPump::spawn(events_rx, tray_tx, ui);
        drop(events_tx);

        pump.shutdown(); // hangs if the pump thread doesn't exit on its own
    }
}
