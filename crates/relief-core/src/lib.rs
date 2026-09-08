//! Grayscale depth map to watertight bas-relief solid.
//!
//! A port of the geometry in `prs-eth/depth-to-3d-print` (Marigold Team, ETH
//! Zürich, Apache-2.0), which is pure arithmetic — no machine learning is
//! involved anywhere in this crate. A depth map is a height field: one pixel
//! becomes one vertex, two triangles per pixel quad, and the sheet is sealed
//! into a printable solid by a frame.
//!
//! # What this crate does differently from the original
//!
//! * **Nothing is materialised to export.** [`Solid`] is a *view* over a
//!   [`HeightField`]: it can report its vertex and triangle counts up front and
//!   hand out any vertex position on demand, so the exporters in [`export`]
//!   stream a mesh of any size through a fixed-size buffer. Peak memory is the
//!   height field, not the mesh.
//! * **The preview is chunked.** [`tiles`] cuts the surface into blocks small
//!   enough to respect a GPU buffer limit (and to upload across several frames),
//!   with normals taken from the height field so tile seams are invisible.
//! * **Watertight by construction.** The original duplicates every seam vertex
//!   and repairs it afterwards with `trimesh.merge_vertices()`. Here the border
//!   indices are shared when the solid is laid out, so the mesh is closed by
//!   definition — see [`check::inspect`], which asserts it.
//! * **The grid is centred symmetrically.** The original places vertices at
//!   `i / (d_max - 1)` but half-widths at `w / (2 * (d_max - 1))`, so the relief
//!   sits half a pixel off-centre inside its frame. See [`mesh::Layout`].
//! * **A frame of zero width is no frame.** Upstream still builds a rim, a
//!   zero-area bezel and an outer wall. Here the relief's sides drop straight to
//!   the back plate instead, so there are no degenerate slivers in the file.
//!
//! # Coordinate space
//!
//! One space, used everywhere: **x right, y up, z out of the relief towards the
//! viewer**, in millimetres — every physical parameter is millimetres too, so
//! nothing is scaled on the way in. The relief occupies `z` in
//! `[-emboss_mm, 0]`, the frame face sits at `z = frame_near_mm`, and the back
//! plate at `z = -(emboss_mm + frame_back_mm)`.
//!
//! [`Params::units`] does not enter into any of that: it scales the numbers
//! written into an exported file and nothing else.
//!
//! It is simultaneously the Bevy view space (Y-up, camera on +Z looking at the
//! relief) and the "face up on the bed" print orientation, so neither the
//! viewport nor the default export needs a transform. Only
//! [`Orientation::Upright`] rotates anything.

pub mod check;
pub mod decimate;
pub mod depth;
pub mod export;
pub mod mesh;
pub mod params;
pub mod sample;
pub mod stats;
pub mod tiles;

pub use decimate::Decimation;
pub use depth::{BitDepth, DepthMap, HeightField};
pub use export::Format;
pub use mesh::{Layout, Solid};
pub use params::{Orientation, Params, Units};
pub use stats::Stats;

/// Everything that can go wrong turning an image into a solid.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("that file is not an image this app can read: {0}")]
    Decode(#[from] image::ImageError),

    #[error("the depth map is {width}×{height}; it must be at least 2×2")]
    TooSmall { width: u32, height: u32 },

    #[error(
        "the near plane ({near}) must be below the far plane ({far}) — nothing is left between them"
    )]
    EmptyDepthWindow { near: f32, far: f32 },

    #[error(
        "the photo is {photo_w}×{photo_h} but the depth map is {depth_w}×{depth_h}; \
         their shapes differ too much to line up"
    )]
    AspectMismatch {
        photo_w: u32,
        photo_h: u32,
        depth_w: u32,
        depth_h: u32,
    },

    #[error("writing the mesh failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Shorthand for this crate's results.
pub type Result<T> = std::result::Result<T, Error>;

/// Run `f` over each row of `data` in parallel where the platform has threads.
///
/// Both arms are given the row index and that row's slice. wasm has no threads
/// without cross-origin isolation, so it takes the serial path; the bounds
/// differ between the two because only rayon needs `Send + Sync`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn each_row_mut<T: Send>(
    data: &mut [T],
    row_len: usize,
    f: impl Fn(usize, &mut [T]) + Send + Sync,
) {
    use rayon::prelude::*;
    data.par_chunks_mut(row_len)
        .enumerate()
        .for_each(|(j, row)| f(j, row));
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn each_row_mut<T>(data: &mut [T], row_len: usize, f: impl Fn(usize, &mut [T])) {
    for (j, row) in data.chunks_mut(row_len).enumerate() {
        f(j, row);
    }
}
