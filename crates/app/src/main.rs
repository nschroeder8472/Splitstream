//! Minimal dev binary: load config, start the engine, wait for ctrl-c.
//! Tray/UI/autostart/single-instance are P4 (`app-shell.md`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use control::{diff, load, ConfigDelta, ConfigWatcher};
use engine::ports::AudioSystem;
use win_audio::WasapiSystem;

/// No platform-config-directory convention has been decided yet (that's a
/// P4/app-shell concern — hot-reload *surface*, not this minimal binary).
/// Defaults to the current directory; override with a path argument.
fn config_path() -> PathBuf {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("splitstream.toml"))
}

fn main() {
    let path = config_path();

    let snapshot = load(&path).unwrap_or_else(|e| {
        eprintln!("failed to load {}: {e:?}", path.display());
        std::process::exit(1);
    });

    let sys: Arc<dyn AudioSystem> = Arc::new(WasapiSystem::new("Splitstream Bus"));
    let handle = engine::start(&snapshot, sys).unwrap_or_else(|e| {
        eprintln!("failed to start engine: {e:?}");
        std::process::exit(1);
    });
    println!(
        "Splitstream engine running ({} group(s)). Ctrl+C to stop.",
        snapshot.groups.len()
    );

    let (watcher, config_rx) = ConfigWatcher::spawn(&path).unwrap_or_else(|e| {
        eprintln!("failed to start config watcher: {e:?}");
        // Process is exiting either way; skip a clean engine shutdown here
        // rather than fight the borrow checker over `handle` for no benefit.
        std::process::exit(1);
    });

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
            .expect("failed to install Ctrl+C handler");
    }

    let mut current = snapshot;
    while !stop.load(Ordering::Relaxed) {
        match config_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(new_snapshot) => {
                match diff(&current, &new_snapshot) {
                    ConfigDelta::Unchanged => {}
                    ConfigDelta::Params(cmds) => {
                        if let Err(e) = handle.apply_params(&cmds) {
                            eprintln!("apply_params failed: {e:?}");
                        }
                    }
                    ConfigDelta::Structural => {
                        if let Err(e) = handle.rebuild(&new_snapshot) {
                            eprintln!("rebuild failed: {e:?}");
                        }
                    }
                }
                current = new_snapshot;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break, // watcher thread died
        }
    }

    drop(watcher);
    if let Err(e) = handle.shutdown() {
        eprintln!("shutdown error: {e:?}");
    }
    println!("Splitstream stopped.");
}
