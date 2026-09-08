//! Turning parameters into a preview, and into a file.
//!
//! Two paths, deliberately different:
//!
//! * The **preview** is capped at [`PREVIEW_MAX_PX`] and rebuilt on a 120 ms
//!   debounce. On desktop it runs on the compute task pool, so dragging a slider
//!   never stutters; on wasm there are no threads, and 512 px is chosen to be
//!   quick enough to build inline.
//! * The **export** runs at the full requested resolution, streams straight to
//!   the file or download, and reports progress. It never becomes a mesh in
//!   memory, so its only real limit is what a slicer will open.

use std::sync::Arc;

use bevy::prelude::*;
use relief_core::depth::Albedo;
use relief_core::params::Params;
use relief_core::{Decimation, DepthMap, Solid, Stats, export, tiles};

use crate::platform::{self, Saved};
use crate::state::{
    AppState, Built, Depth, PREVIEW_MAX_PX, Photo, Status, TILE_VERTS, ready_to_rebuild,
};

/// Everything a build needs, detached from the ECS so it can cross a thread.
struct Job {
    depth: Arc<DepthMap>,
    photo: Option<Photo>,
    params: Params,
}

impl Job {
    fn new(state: &AppState) -> Self {
        Self {
            depth: state.depth.map.clone(),
            photo: state.photo.clone(),
            params: state.preview_params(),
        }
    }

    /// The actual work: condition, colour, chunk.
    fn run(self) -> Result<Built, String> {
        let field = self
            .depth
            .to_height_field(&self.params)
            .map_err(|e| e.to_string())?;

        // Only for the export-colour preflight and the vertex colours an
        // export needs; the on-screen relief is textured with the photo
        // itself, at photo resolution rather than grid resolution.
        let albedo = self.photo.as_ref().and_then(|photo| {
            // A photo that fails to decode here was decoded once already when
            // it was opened, so this is unreachable in practice; drop it rather
            // than fail a rebuild over it.
            Albedo::decode(&photo.bytes, field.w, field.h).ok()
        });

        let decimation = Decimation::build(&field, self.params.tolerance_mm);
        let solid = Solid::new(&field, &self.params)
            .with_albedo(albedo.as_ref())
            .with_decimation(decimation.as_ref());
        let stats = Stats::new(&solid, export::Format::Stl, &self.params);
        let tiles = tiles::preview(&solid, TILE_VERTS);

        Ok(Built {
            field: Arc::new(field),
            stats,
            tiles,
        })
    }
}

/// Desktop: hand the job to the compute pool and pick it up when it lands.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource, Default)]
pub struct Worker(Option<bevy::tasks::Task<Result<Built, String>>>);

#[cfg(not(target_arch = "wasm32"))]
pub fn drive(mut state: ResMut<AppState>, time: Res<Time>, mut worker: ResMut<Worker>) {
    use bevy::tasks::{AsyncComputeTaskPool, block_on};

    // Collect a finished build first, so its result is on screen this frame.
    if worker.0.as_ref().is_some_and(|task| task.is_finished()) {
        let task = worker.0.take().expect("just checked");
        finish(&mut state, block_on(task));
    }

    let now = time.elapsed_secs_f64();
    if worker.0.is_some() || !ready_to_rebuild(&state, now) {
        return;
    }
    state.dirty_since = None;
    let job = Job::new(&state);
    worker.0 = Some(AsyncComputeTaskPool::get().spawn(async move { job.run() }));
}

/// Web: no threads, so build inline. The preview cap is what makes that fine.
#[cfg(target_arch = "wasm32")]
pub fn drive(mut state: ResMut<AppState>, time: Res<Time>) {
    let now = time.elapsed_secs_f64();
    if !ready_to_rebuild(&state, now) {
        return;
    }
    state.dirty_since = None;
    let result = Job::new(&state).run();
    finish(&mut state, result);
}

fn finish(state: &mut AppState, result: Result<Built, String>) {
    match result {
        Err(message) => state.status = Status::error(message),
        Ok(built) => {
            let stats = built.stats.clone();
            state.built = Some(built);
            state.applied = Some(state.preview_params());
            state.generation += 1;
            // The depth pane is drawn from the conditioned field, so it has to
            // be rebuilt whenever the field is.
            state.depth_texture = None;

            // Only speak up when there is something to say; otherwise leave
            // whatever the last real message was on screen.
            if !stats.steps_ok() {
                state.status = Status::warn(stats.step_line());
            } else if stats.slicer_strain() {
                state.status = Status::warn(format!(
                    "{} triangles at {} px — more than most slicers handle comfortably",
                    stats.triangles, state.params.size_px
                ));
            }
        }
    }
}

/// Upload the photo as a texture when it changes.
pub fn sync_photo_texture(mut state: ResMut<AppState>, mut images: ResMut<Assets<Image>>) {
    if !state.photo_dirty {
        return;
    }
    state.photo_dirty = false;

    let Some(photo) = state.photo.clone() else {
        state.photo_texture = None;
        return;
    };
    match image::load_from_memory(&photo.bytes) {
        Err(e) => state.status = Status::error(format!("{}: {e}", photo.name)),
        Ok(decoded) => {
            use bevy::asset::RenderAssetUsages;
            use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};

            let rgba = decoded.to_rgba8();
            let (width, height) = rgba.dimensions();
            let image = Image::new(
                Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                TextureDimension::D2,
                rgba.into_raw(),
                TextureFormat::Rgba8UnormSrgb,
                // The CPU copy is not needed once it is on the GPU, and at photo
                // resolution it is worth several megabytes.
                RenderAssetUsages::RENDER_WORLD,
            );
            state.photo_texture = Some(images.add(image));
        }
    }
}

/// Build at full resolution and stream it out.
///
/// Runs on the main thread on purpose: the file dialog has to, and the write is
/// the kind of operation a person expects to wait for. Progress goes to the
/// status line so a long export says so.
pub fn export_now(state: &mut AppState) {
    let params = state.params;
    let format = state.format;

    let field = match state.depth.map.to_height_field(&params) {
        Err(e) => {
            state.status = Status::error(e.to_string());
            return;
        }
        Ok(field) => field,
    };
    let albedo = state.photo.as_ref().and_then(|photo| {
        if format.carries_colour() {
            Albedo::decode(&photo.bytes, field.w, field.h).ok()
        } else {
            None
        }
    });
    let decimation = Decimation::build(&field, params.tolerance_mm);
    let solid = Solid::new(&field, &params)
        .with_albedo(albedo.as_ref())
        .with_decimation(decimation.as_ref());
    let stats = Stats::new(&solid, format, &params);

    let name = state.suggested_name();
    let outcome = platform::save_stream(&name, |sink| {
        export::write(format, &solid, sink, &mut |_| {}).map_err(|e| e.to_string())
    });

    state.status = match outcome {
        Err(message) => Status::error(message),
        Ok(Saved::Cancelled) => Status::info("Export cancelled"),
        Ok(Saved::Downloaded) => Status::info(format!(
            "Downloaded {name} — {} triangles, {}",
            stats.triangles,
            stats.file_line()
        )),
        Ok(Saved::To(path)) => Status::info(format!(
            "Wrote {path} — {} triangles, {}",
            stats.triangles,
            stats.file_line()
        )),
    };
}

/// Load the built-in sample, at whatever bit depth is asked for.
pub fn load_sample(state: &mut AppState, eight_bit: bool, now: f64) {
    use relief_core::depth::BitDepth;
    use relief_core::sample;

    let (bits, name) = if eight_bit {
        (BitDepth::Eight, "sample_photo_gray.jpg")
    } else {
        (BitDepth::Sixteen, "sample_terrain_16bit.png")
    };
    state.depth = Depth {
        name: name.into(),
        map: Arc::new(sample::depth_map_with(1536, 1152, bits)),
    };
    state.photo = None;
    state.photo_texture = None;
    state.status = Status::info(if eight_bit {
        "Synthetic 8-bit sample with JPEG-style blocks — raise Smooth radius to see it cleaned up"
    } else {
        "Synthetic 16-bit sample"
    });
    state.touch(now);
}

/// The largest resolution this build will let you export.
///
/// The desktop number is where a slicer stops coping; the web number is where
/// the height field itself — 4 bytes a pixel, plus the conditioning copies —
/// starts crowding a 32-bit address space.
pub const fn export_max_px() -> u32 {
    #[cfg(target_arch = "wasm32")]
    {
        2048
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        relief_core::params::SIZE_PX_MAX
    }
}

/// What the preview is showing, for the status line.
pub fn preview_note(state: &AppState) -> String {
    match state.built.as_ref() {
        None => "building…".into(),
        Some(built) => {
            let (w, h) = built.stats.grid;
            let tiles = built.tiles.len().saturating_sub(1);
            let capped = state.params.size_px > PREVIEW_MAX_PX;
            format!(
                "preview {w}×{h} in {tiles} chunk{}{}",
                if tiles == 1 { "" } else { "s" },
                if capped { ", export is finer" } else { "" }
            )
        }
    }
}
