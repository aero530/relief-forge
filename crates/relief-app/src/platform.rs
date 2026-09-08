//! Files and stored settings, on desktop and in the browser.
//!
//! # Saving is a stream, not a buffer
//!
//! Every export goes through [`save_stream`], which hands the caller an
//! [`std::io::Write`] and never sees the mesh. On desktop that writer is a file.
//! In the browser it is [`ChunkSink`], which accumulates 4 MB at a time and
//! pushes each chunk into a JS array that becomes a `Blob` — the browser owns
//! the bytes, so a 300 MB STL never has to exist inside the 32-bit wasm heap.
//! That is the whole reason the web build has no export size limit.
//!
//! # Saving is synchronous, opening is not
//!
//! Saving blocks: `rfd`'s native dialog returns a path, and a browser download
//! needs no answer. Opening cannot, because on the web the file only arrives
//! after the user has picked it, in a future the frame loop cannot await. Rather
//! than have two shapes, *both* targets deliver through an [`Inbox`] the UI
//! drains each frame — the native side simply fills it before returning.

use std::io::Write;
use std::sync::{Arc, Mutex};

/// Which slot an opened file is destined for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickKind {
    Depth,
    Photo,
    Settings,
}

impl PickKind {
    fn filter(self) -> (&'static str, &'static [&'static str]) {
        match self {
            PickKind::Depth => ("Depth map", &["png", "jpg", "jpeg"]),
            PickKind::Photo => ("Image", &["png", "jpg", "jpeg"]),
            PickKind::Settings => ("Settings", &["json"]),
        }
    }
}

// `Downloaded` on desktop and `To` on the web are each dead on the other
// target; naming both on both keeps the call sites free of `cfg`.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// Written to this path (desktop only).
    To(String),
    /// Handed to the browser as a download (web only).
    Downloaded,
    /// The user dismissed the dialog.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    File {
        kind: PickKind,
        name: String,
        bytes: Vec<u8>,
    },
    Cancelled,
}

/// Where an in-flight file pick delivers its result.
///
/// Cloneable and shared: the wasm side hands a clone to a spawned future, and
/// the UI keeps the original to drain. A `Mutex` rather than a `RefCell` because
/// Bevy requires its resources to be `Send + Sync` on every target.
#[derive(Clone, Default)]
pub struct Inbox(Arc<Mutex<Option<Result<Opened, String>>>>);

impl Inbox {
    pub fn take(&self) -> Option<Result<Opened, String>> {
        self.0.lock().ok()?.take()
    }

    fn put(&self, result: Result<Opened, String>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(result);
        }
    }
}

// ---------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
pub fn open(inbox: &Inbox, kind: PickKind) {
    let (label, extensions) = kind.filter();
    let picked = rfd::FileDialog::new()
        .add_filter(label, extensions)
        .pick_file();

    inbox.put(match picked {
        None => Ok(Opened::Cancelled),
        Some(path) => std::fs::read(&path)
            .map(|bytes| Opened::File {
                kind,
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
                bytes,
            })
            .map_err(|e| format!("reading {}: {e}", path.display())),
    });
}

#[cfg(target_arch = "wasm32")]
pub fn open(inbox: &Inbox, kind: PickKind) {
    let inbox = inbox.clone();
    let (label, extensions) = kind.filter();
    wasm_bindgen_futures::spawn_local(async move {
        let picked = rfd::AsyncFileDialog::new()
            .add_filter(label, extensions)
            .pick_file()
            .await;

        inbox.put(match picked {
            None => Ok(Opened::Cancelled),
            Some(handle) => Ok(Opened::File {
                kind,
                name: handle.file_name(),
                bytes: handle.read().await,
            }),
        });
    });
}

// ---------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------

/// Ask for a destination, then let `write` stream to it.
///
/// `write` may be called with a buffer that flushes as it fills, so it must not
/// assume seeking or rewinding.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_stream(
    suggested_name: &str,
    write: impl FnOnce(&mut dyn Write) -> Result<(), String>,
) -> Result<Saved, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_file_name(suggested_name)
        .save_file()
    else {
        return Ok(Saved::Cancelled);
    };

    let mut file =
        std::fs::File::create(&path).map_err(|e| format!("creating {}: {e}", path.display()))?;
    write(&mut file)?;
    file.flush()
        .map_err(|e| format!("finishing {}: {e}", path.display()))?;
    Ok(Saved::To(path.display().to_string()))
}

/// Stream into a `Blob` and hand it to the browser as a download.
///
/// The object URL is revoked immediately — otherwise it leaks for the lifetime
/// of the tab, and for a 300 MB export that is 300 MB.
#[cfg(target_arch = "wasm32")]
pub fn save_stream(
    suggested_name: &str,
    write: impl FnOnce(&mut dyn Write) -> Result<(), String>,
) -> Result<Saved, String> {
    use wasm_bindgen::JsCast;

    let describe = |what: &str, e: wasm_bindgen::JsValue| format!("{what}: {e:?}");

    let mut sink = ChunkSink::new();
    write(&mut sink)?;
    let parts = sink.finish();

    let blob = web_sys::Blob::new_with_u8_array_sequence(&parts)
        .map_err(|e| describe("assembling the download", e))?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)
        .map_err(|e| describe("creating object URL", e))?;

    let result = (|| {
        let document = web_sys::window()
            .and_then(|w| w.document())
            .ok_or_else(|| "no document".to_string())?;
        let anchor = document
            .create_element("a")
            .map_err(|e| describe("creating anchor", e))?
            .dyn_into::<web_sys::HtmlAnchorElement>()
            .map_err(|_| "element is not an anchor".to_string())?;
        anchor.set_href(&url);
        anchor.set_download(suggested_name);
        anchor.click();
        Ok(Saved::Downloaded)
    })();

    let _ = web_sys::Url::revoke_object_url(&url);
    result
}

/// A `Write` that hands fixed-size chunks to JavaScript.
///
/// 4 MB per chunk: big enough that the JS boundary crossing is noise, small
/// enough that the Rust-side buffer is never a problem.
#[cfg(target_arch = "wasm32")]
pub struct ChunkSink {
    parts: js_sys::Array,
    buffer: Vec<u8>,
}

#[cfg(target_arch = "wasm32")]
impl ChunkSink {
    const CHUNK: usize = 4 << 20;

    pub fn new() -> Self {
        Self {
            parts: js_sys::Array::new(),
            buffer: Vec::with_capacity(Self::CHUNK),
        }
    }

    fn hand_over(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let view = js_sys::Uint8Array::from(self.buffer.as_slice());
        self.parts.push(&view);
        self.buffer.clear();
    }

    pub fn finish(mut self) -> js_sys::Array {
        self.hand_over();
        self.parts
    }
}

#[cfg(target_arch = "wasm32")]
impl Write for ChunkSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(data);
        if self.buffer.len() >= Self::CHUNK {
            self.hand_over();
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Deliberately *not* handing over here: `export::write` flushes its own
        // BufWriter at the end of every file, and chunking on that would make
        // one tiny final part per export. `finish` is the real flush.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Settings remembered between sessions
// ---------------------------------------------------------------------------

const REMEMBERED: &str = "settings.json";

#[cfg(not(target_arch = "wasm32"))]
fn config_path() -> Option<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "aero530", "relief-forge")?;
    Some(dirs.config_dir().join(REMEMBERED))
}

/// Where the remembered settings live, phrased for a person to read.
///
/// Settings that reappear by themselves are a mystery until you know where they
/// came from, and on desktop this is the only way to find the file if you want
/// it gone.
#[cfg(not(target_arch = "wasm32"))]
pub fn remembered_location() -> Option<String> {
    Some(config_path()?.display().to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn recall() -> Option<String> {
    std::fs::read_to_string(config_path()?).ok()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn remember(text: &str) -> Result<(), String> {
    let path = config_path().ok_or("no config directory on this system")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, text).map_err(|e| format!("writing {}: {e}", path.display()))
}

#[cfg(target_arch = "wasm32")]
pub fn remembered_location() -> Option<String> {
    Some("this browser's local storage".into())
}

#[cfg(target_arch = "wasm32")]
fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

#[cfg(target_arch = "wasm32")]
pub fn recall() -> Option<String> {
    storage()?.get_item(REMEMBERED).ok()?
}

#[cfg(target_arch = "wasm32")]
pub fn remember(text: &str) -> Result<(), String> {
    storage()
        .ok_or("this browser has no local storage available")?
        .set_item(REMEMBERED, text)
        .map_err(|e| format!("local storage refused the write: {e:?}"))
}
