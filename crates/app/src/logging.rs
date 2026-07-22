//! File logging + panic surface (simple-launch.md L4). Once `main.rs` flips
//! to the Windows GUI subsystem, `eprintln!`/stderr diagnostics and a bare
//! panic message go nowhere — there's no console to receive them
//! (operational-learnings 2026-07-20). This is the replacement surface:
//! a rotating log file, plus a native dialog on panic.

use std::panic;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;

/// Must be held for the process lifetime — dropping it stops the background
/// writer thread and drops any buffered-but-unflushed log lines.
pub fn init(log_dir: &Path) -> WorkerGuard {
    let appender = rolling::daily(log_dir, "splitstream.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    tracing_subscriber::fmt().with_writer(writer).with_ansi(false).init();

    install_panic_hook();
    guard
}

/// Logs the panic to the rotating file (so it's captured even off a
/// double-clicked, consoleless launch), then surfaces a native dialog — the
/// GUI-subsystem counterpart to a console panic message nobody would see.
fn install_panic_hook() {
    panic::set_hook(Box::new(|info| {
        let message = info.to_string();
        tracing::error!("panic: {message}");
        rfd::MessageDialog::new()
            .set_title("Splitstream crashed")
            .set_description(&message)
            .set_level(rfd::MessageLevel::Error)
            .set_buttons(rfd::MessageButtons::Ok)
            .show();
    }));
}

/// Surfaces a fatal startup error (config/engine/device failure) as a native
/// dialog instead of a silent `exit(1)` into an unseen console — the
/// non-panic counterpart to `install_panic_hook`, for errors handled
/// gracefully rather than unwound.
pub fn fatal_dialog(context: &str, detail: &str) {
    tracing::error!("fatal: {context}: {detail}");
    rfd::MessageDialog::new()
        .set_title("Splitstream failed to start")
        .set_description(format!("{context}: {detail}"))
        .set_level(rfd::MessageLevel::Error)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}
