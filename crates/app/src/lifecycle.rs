//! Process lifecycle: single-instance enforcement + second-launch signaling,
//! and per-user autostart registration. Both via wrapper crates
//! (`single-instance`, `auto-launch`, `interprocess`) — `app` never imports
//! `windows`/`windows-rs` directly (app-shell.md constraint).

use std::io::Write;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use auto_launch::AutoLaunchBuilder;
use interprocess::local_socket::traits::{ListenerExt, Stream};
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Stream as LocalSocketStream, ToNsName};
use single_instance::SingleInstance;

/// Shared identity for the named mutex/pipe (`InstanceGuard`) and the
/// autostart registration (`set_autostart`) — one app, one name.
pub const APP_ID: &str = "Splitstream";

#[derive(Debug)]
pub enum ShellError {
    Instance(String),
    Autostart(String),
    Hotkey(String),
}

/// Opaque wake-up: the second instance connected and asked to be surfaced.
/// Carries no payload — the settings window is always what gets shown.
pub struct SurfaceSignal;

/// Held for the process lifetime; the named mutex it wraps releases on drop.
pub struct InstanceGuard {
    _mutex: SingleInstance,
}

pub enum InstanceOutcome {
    Primary(InstanceGuard, Receiver<SurfaceSignal>),
    /// Already signaled the primary instance — caller should exit immediately.
    Secondary,
}

impl InstanceGuard {
    pub fn acquire(app_id: &str) -> Result<InstanceOutcome, ShellError> {
        let mutex = SingleInstance::new(app_id).map_err(|e| ShellError::Instance(e.to_string()))?;
        if mutex.is_single() {
            let rx = spawn_signal_listener(app_id)?;
            Ok(InstanceOutcome::Primary(InstanceGuard { _mutex: mutex }, rx))
        } else {
            signal_primary_instance(app_id);
            Ok(InstanceOutcome::Secondary)
        }
    }
}

fn socket_name(app_id: &str) -> Result<interprocess::local_socket::Name<'static>, ShellError> {
    format!("{app_id}-surface.sock")
        .to_ns_name::<GenericNamespaced>()
        .map_err(|e| ShellError::Instance(e.to_string()))
}

/// One thread, blocking accept loop: every successful connection is a
/// surface request, regardless of what (if anything) the client writes.
fn spawn_signal_listener(app_id: &str) -> Result<Receiver<SurfaceSignal>, ShellError> {
    let name = socket_name(app_id)?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_sync()
        .map_err(|e| ShellError::Instance(e.to_string()))?;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let conn: std::io::Result<LocalSocketStream> = conn;
            if conn.is_err() {
                continue;
            }
            if tx.send(SurfaceSignal).is_err() {
                return; // primary instance shutting down
            }
        }
    });
    Ok(rx)
}

/// Best-effort: if the primary instance's listener isn't reachable for any
/// reason, this instance still exits (per `InstanceOutcome::Secondary`
/// contract) — it just won't have surfaced the other window.
fn signal_primary_instance(app_id: &str) {
    let Ok(name) = socket_name(app_id) else { return };
    if let Ok(mut stream) = LocalSocketStream::connect(name) {
        let _ = stream.write_all(&[1]);
    }
}

/// Per-user registration (no elevation) via `auto-launch`'s cross-platform
/// builder — the current exe's own path, no launch args.
pub fn set_autostart(enabled: bool) -> Result<(), ShellError> {
    let exe = std::env::current_exe().map_err(|e| ShellError::Autostart(e.to_string()))?;
    let auto = AutoLaunchBuilder::new()
        .set_app_name(APP_ID)
        .set_app_path(&exe.to_string_lossy())
        .set_args(&[] as &[&str])
        .build()
        .map_err(|e| ShellError::Autostart(e.to_string()))?;

    let result = if enabled { auto.enable() } else { auto.disable() };
    result.map_err(|e| ShellError::Autostart(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// Named mutex/pipe are OS-wide resources — tests run in parallel threads
    /// of the same process (`cargo test` default), so each test needs its own
    /// name to avoid colliding with the others.
    fn unique_app_id(case: &str) -> String {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("splitstream-test-{case}-{}-{n}", std::process::id())
    }

    #[test]
    fn first_acquire_is_primary() {
        let id = unique_app_id("first-primary");
        let outcome = InstanceGuard::acquire(&id).unwrap();
        assert!(matches!(outcome, InstanceOutcome::Primary(..)));
    }

    #[test]
    fn second_acquire_with_the_same_id_is_secondary() {
        let id = unique_app_id("second-secondary");
        let first = InstanceGuard::acquire(&id).unwrap();
        assert!(matches!(first, InstanceOutcome::Primary(..)));

        let second = InstanceGuard::acquire(&id).unwrap();
        assert!(matches!(second, InstanceOutcome::Secondary));
    }

    #[test]
    fn secondary_acquire_wakes_the_primarys_receiver() {
        let id = unique_app_id("wakes-primary");
        let InstanceOutcome::Primary(_guard, rx) = InstanceGuard::acquire(&id).unwrap() else {
            panic!("expected Primary");
        };

        let second = InstanceGuard::acquire(&id).unwrap();
        assert!(matches!(second, InstanceOutcome::Secondary));

        rx.recv_timeout(Duration::from_secs(2))
            .expect("primary should receive a SurfaceSignal after a secondary acquire");
    }

    /// Touches the real Windows registry Run key — not run by default, same
    /// pattern as win-audio's real-hardware `#[ignore]` tests. Run explicitly
    /// during development: `cargo test -p app --lib -- --ignored autostart`.
    #[test]
    #[ignore]
    fn autostart_can_be_enabled_then_disabled() {
        set_autostart(true).unwrap();
        set_autostart(false).unwrap();
    }
}
