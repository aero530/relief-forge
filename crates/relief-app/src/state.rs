//! What the app knows, and how it gets it.

use std::sync::Arc;

use bevy::prelude::*;
use bevy_egui::egui;
use relief_core::depth::{DepthMap, HeightField, check_aspect};
use relief_core::export::Format;
use relief_core::params::Params;
use relief_core::{Stats, sample};

use crate::platform::{self, Inbox, Opened, PickKind};

/// Grid cap for the live preview.
///
/// The preview exists to judge the shape, and 512 px of relief already reads as
/// detailed at any sensible window size. Export resolution is separate and much
/// higher — that split is what keeps a slider drag responsive.
pub const PREVIEW_MAX_PX: u32 = 512;

/// Vertices per preview chunk. See `relief_core::tiles`.
pub const TILE_VERTS: usize = relief_core::tiles::DEFAULT_MAX_TILE_VERTS;

/// Idle time before a parameter change triggers a rebuild.
const REBUILD_DEBOUNCE: f64 = 0.12;
/// Idle time before settings are written to disk.
const AUTOSAVE_DEBOUNCE: f64 = 1.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tone {
    #[default]
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub text: String,
    pub tone: Tone,
}

impl Status {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: Tone::Info,
        }
    }

    pub fn warn(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: Tone::Warn,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: Tone::Error,
        }
    }
}

/// The loaded depth map. `Arc` because a rebuild hands it to a worker thread.
#[derive(Clone)]
pub struct Depth {
    pub name: String,
    pub map: Arc<DepthMap>,
}

/// The optional photo, kept as bytes so it can be re-sampled to whatever grid
/// the current parameters produce.
#[derive(Clone)]
pub struct Photo {
    pub name: String,
    pub bytes: Arc<Vec<u8>>,
    pub width: u32,
    pub height: u32,
}

/// What the last completed rebuild produced.
pub struct Built {
    pub field: Arc<HeightField>,
    pub stats: Stats,
    pub tiles: Vec<relief_core::tiles::TileMesh>,
}

#[derive(Resource)]
pub struct AppState {
    pub params: Params,
    /// The parameters the current preview was built from.
    pub applied: Option<Params>,
    pub depth: Depth,
    pub photo: Option<Photo>,
    pub inbox: Inbox,
    pub status: Status,
    pub format: Format,
    /// Tint each preview chunk differently, to show the tiling.
    pub show_tiles: bool,
    /// Set when something changed; a rebuild starts once it stops changing.
    pub dirty_since: Option<f64>,
    pub save_since: Option<f64>,
    /// Bumped when a new preview lands, so the viewport knows to re-upload.
    pub generation: u64,
    pub built: Option<Built>,
    /// The conditioned depth map, as the left pane shows it.
    pub depth_texture: Option<egui::TextureHandle>,
    pub photo_texture: Option<Handle<Image>>,
    pub photo_dirty: bool,
}

impl Default for AppState {
    fn default() -> Self {
        let map = sample::depth_map(1536, 1152);
        Self {
            params: Params::default(),
            applied: None,
            depth: Depth {
                name: "sample_terrain_16bit.png".into(),
                map: Arc::new(map),
            },
            photo: None,
            inbox: Inbox::default(),
            status: Status::info("Sample depth map loaded — open your own to replace it."),
            format: Format::Stl,
            show_tiles: false,
            dirty_since: Some(0.0),
            save_since: None,
            generation: 0,
            built: None,
            depth_texture: None,
            photo_texture: None,
            photo_dirty: false,
        }
    }
}

impl AppState {
    /// Preview grid: the requested resolution, capped.
    pub fn preview_params(&self) -> Params {
        Params {
            size_px: self.params.size_px.min(PREVIEW_MAX_PX),
            ..self.params
        }
    }

    /// Note that something the geometry depends on has changed.
    pub fn touch(&mut self, now: f64) {
        self.dirty_since = Some(now);
        self.save_since = Some(now);
    }

    pub fn stats(&self) -> Option<&Stats> {
        self.built.as_ref().map(|b| &b.stats)
    }

    /// Filename to suggest for an export.
    pub fn suggested_name(&self) -> String {
        let stem = self
            .depth
            .name
            .rsplit_once('.')
            .map(|(stem, _)| stem)
            .unwrap_or(&self.depth.name)
            .trim_end_matches("_16bit")
            .trim_end_matches("_depth");
        format!("{stem}.{}", self.format.extension())
    }
}

/// Bring back the settings from the last session.
pub fn restore(mut state: ResMut<AppState>) {
    let Some(text) = platform::recall() else {
        return;
    };
    match serde_json::from_str::<Params>(&text) {
        Ok(mut params) => {
            params.clamp();
            if params.validate().is_ok() {
                state.params = params;
                state.status = Status::info(match platform::remembered_location() {
                    Some(where_from) => format!("Restored your last settings from {where_from}"),
                    None => "Restored your last settings".into(),
                });
            }
        }
        // Not worth bothering anyone about: a corrupt settings file just means
        // starting from the defaults, which is where a first run starts anyway.
        Err(e) => info!("ignoring unreadable settings: {e}"),
    }
}

/// Drain whatever a file picker delivered.
pub fn apply_opened(mut state: ResMut<AppState>, time: Res<Time>) {
    let Some(result) = state.inbox.take() else {
        return;
    };
    let now = time.elapsed_secs_f64();

    match result {
        Err(message) => state.status = Status::error(message),
        Ok(Opened::Cancelled) => {}
        Ok(Opened::File { kind, name, bytes }) => match kind {
            PickKind::Depth => match DepthMap::decode(&bytes) {
                Err(e) => state.status = Status::error(format!("{name}: {e}")),
                Ok(map) => {
                    let mut note =
                        format!("{name} — {}×{} {}", map.width, map.height, map.bits.label());
                    let mut tone = Tone::Info;
                    if map.was_colour {
                        note.push_str(
                            ", colour converted to luma — a depth map should be grayscale",
                        );
                        tone = Tone::Warn;
                    }
                    // A photo already loaded against the previous depth map may
                    // not fit this one.
                    let photo_fits = state
                        .photo
                        .as_ref()
                        .map(|p| check_aspect(&map, p.width, p.height).is_ok());
                    if photo_fits == Some(false) {
                        note.push_str("; the loaded photo no longer matches and was dropped");
                        tone = Tone::Warn;
                        state.photo = None;
                        state.photo_texture = None;
                    }
                    state.depth = Depth {
                        name,
                        map: Arc::new(map),
                    };
                    state.status = Status { text: note, tone };
                    state.touch(now);
                }
            },
            PickKind::Photo => match image::load_from_memory(&bytes) {
                Err(e) => state.status = Status::error(format!("{name}: {e}")),
                Ok(decoded) => {
                    let (width, height) = (decoded.width(), decoded.height());
                    match check_aspect(&state.depth.map, width, height) {
                        Err(e) => state.status = Status::error(e.to_string()),
                        Ok(()) => {
                            state.photo = Some(Photo {
                                name: name.clone(),
                                bytes: Arc::new(bytes),
                                width,
                                height,
                            });
                            state.photo_dirty = true;
                            state.status = Status::info(format!(
                                "{name} — {width}×{height}, colouring the relief"
                            ));
                            state.touch(now);
                        }
                    }
                }
            },
            PickKind::Settings => match String::from_utf8(bytes)
                .map_err(|e| e.to_string())
                .and_then(|text| serde_json::from_str::<Params>(&text).map_err(|e| e.to_string()))
            {
                Err(e) => state.status = Status::error(format!("{name} is not settings JSON: {e}")),
                Ok(mut params) => {
                    params.clamp();
                    match params.validate() {
                        Err(e) => state.status = Status::error(e.to_string()),
                        Ok(()) => {
                            state.params = params;
                            state.status = Status::info(format!("Loaded settings from {name}"));
                            state.touch(now);
                        }
                    }
                }
            },
        },
    }
}

/// Notice a parameter edit and start the debounce clock.
pub fn watch_params(mut state: ResMut<AppState>, time: Res<Time>) {
    let changed = match state.applied {
        None => true,
        Some(applied) => applied != state.preview_params(),
    };
    if changed && state.dirty_since.is_none() {
        let now = time.elapsed_secs_f64();
        state.touch(now);
    }
}

/// Has the dirty flag been still long enough to rebuild?
pub fn ready_to_rebuild(state: &AppState, now: f64) -> bool {
    state
        .dirty_since
        .is_some_and(|since| now - since >= REBUILD_DEBOUNCE)
}

/// Write the settings out once the user stops fiddling.
pub fn autosave(mut state: ResMut<AppState>, time: Res<Time>) {
    let Some(since) = state.save_since else {
        return;
    };
    if time.elapsed_secs_f64() - since < AUTOSAVE_DEBOUNCE {
        return;
    }
    state.save_since = None;
    flush_settings(&state);
}

/// Save on the way out, so a change made in the last second is not lost.
pub fn autosave_on_exit(state: Res<AppState>, mut exits: MessageReader<AppExit>) {
    if exits.read().next().is_some() && state.save_since.is_some() {
        flush_settings(&state);
    }
}

fn flush_settings(state: &AppState) {
    match serde_json::to_string_pretty(&state.params) {
        Ok(text) => {
            if let Err(e) = platform::remember(&text) {
                warn!("could not remember settings: {e}");
            }
        }
        Err(e) => warn!("could not serialise settings: {e}"),
    }
}
