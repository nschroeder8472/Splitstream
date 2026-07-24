//! Session chip icon cache (app-icons.md). Path-keyed and negative-caching
//! (decisions 4, 5): two sessions sharing an exe extract once, and a path
//! that fails is never retried. Extraction runs on a dedicated worker thread
//! (decision 1) — the render thread never blocks on the shell or filesystem,
//! the rule this codebase has broken and fixed before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use eframe::egui;

/// Never exposed — collapsing to `texture() -> Option<TextureHandle>` at the
/// public boundary means "pending", "failed" and "no path" are one case at
/// the call site (decision 11).
enum IconSlot {
    Pending,
    Ready(egui::TextureHandle),
    Failed,
}

/// Path-keyed icon cache plus the worker that fills it. The worker is spawned
/// lazily on the first [`IconCache::poll`] call, the earliest point an
/// `egui::Context` exists to hand it (decision 10) — not in `IconCache::new`.
pub struct IconCache {
    entries: HashMap<PathBuf, IconSlot>,
    request_tx: Sender<PathBuf>,
    request_rx: Option<Receiver<PathBuf>>,
    results_tx: Option<Sender<(PathBuf, Option<win_shell::IconImage>)>>,
    results_rx: Receiver<(PathBuf, Option<win_shell::IconImage>)>,
    worker: Option<JoinHandle<()>>,
}

impl IconCache {
    pub fn new() -> IconCache {
        let (request_tx, request_rx) = mpsc::channel();
        let (results_tx, results_rx) = mpsc::channel();
        IconCache {
            entries: HashMap::new(),
            request_tx,
            request_rx: Some(request_rx),
            results_tx: Some(results_tx),
            results_rx,
            worker: None,
        }
    }

    /// Drains finished extractions and uploads their textures. Spawns the
    /// worker on first call. Call once per frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if self.worker.is_none() {
            if let (Some(requests), Some(results)) = (self.request_rx.take(), self.results_tx.take()) {
                self.worker = Some(spawn_icon_worker(ctx.clone(), requests, results));
            }
        }

        while let Ok((path, result)) = self.results_rx.try_recv() {
            let slot = match result {
                Some(image) => {
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied([image.width as usize, image.height as usize], &image.rgba);
                    let handle =
                        ctx.load_texture(path.to_string_lossy(), color_image, egui::TextureOptions::LINEAR);
                    IconSlot::Ready(handle)
                }
                None => IconSlot::Failed,
            };
            self.entries.insert(path, slot);
        }
    }

    /// This path's icon, enqueueing an extraction the first time it is seen.
    /// `None` means "draw the fallback" — pending, failed, or empty path.
    /// Returns a cloned handle so the caller never holds a borrow of the
    /// cache across rendering.
    pub fn texture(&mut self, path: &Path) -> Option<egui::TextureHandle> {
        if path.as_os_str().is_empty() {
            return None;
        }
        match self.entries.get(path) {
            Some(IconSlot::Ready(handle)) => Some(handle.clone()),
            Some(IconSlot::Pending | IconSlot::Failed) => None,
            None => {
                self.entries.insert(path.to_path_buf(), IconSlot::Pending);
                let _ = self.request_tx.send(path.to_path_buf());
                None
            }
        }
    }
}

impl Default for IconCache {
    fn default() -> Self {
        IconCache::new()
    }
}

/// Blocks on `requests.recv()` — this thread, not the UI thread (decision 1).
/// Exits once `requests` closes (the cache, and with it `request_tx`, dropped).
fn spawn_icon_worker(
    ctx: egui::Context,
    requests: Receiver<PathBuf>,
    results: Sender<(PathBuf, Option<win_shell::IconImage>)>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(path) = requests.recv() {
            let result = win_shell::extract_icon_rgba(&path);
            if results.send((path, result)).is_err() {
                return; // cache dropped while extraction was in flight
            }
            ctx.request_repaint();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_process_path_is_never_enqueued() {
        let mut cache = IconCache::new();

        let handle = cache.texture(Path::new(""));

        assert!(handle.is_none());
        assert!(cache.entries.is_empty(), "an empty path must not create a cache entry or enqueue a request");
    }

    #[test]
    fn a_failed_extraction_is_never_retried() {
        // Decision 5's whole point: negative caching stops a failing path
        // being retried every frame forever.
        let mut cache = IconCache::new();
        let path = PathBuf::from(r"C:\some\path.exe");
        cache.entries.insert(path.clone(), IconSlot::Failed);

        let handle = cache.texture(&path);

        assert!(handle.is_none());
        assert!(
            matches!(cache.entries.get(&path), Some(IconSlot::Failed)),
            "a failed path must stay Failed, never re-enqueued as Pending"
        );
    }

    #[test]
    fn a_pending_path_is_not_enqueued_twice() {
        // Decision 4: two sessions sharing an exe extract once.
        let mut cache = IconCache::new();
        let path = PathBuf::from(r"C:\some\path.exe");

        cache.texture(&path);
        cache.texture(&path);

        assert_eq!(cache.entries.len(), 1, "a second call on the same still-pending path must not add a second entry");
    }
}
