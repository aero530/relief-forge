//! The preview mesh, cut into chunks.
//!
//! One 4096-px relief is 12.6 M vertices — 400 MB of positions alone, which is
//! past WebGPU's default 256 MiB `maxBufferSize` and far past what a 32-bit
//! wasm heap will hand out in one allocation. So the surface is emitted as
//! tiles, each small enough to be one GPU buffer, which also lets a rebuild
//! upload across several frames instead of stalling on one giant allocation,
//! and gives the renderer something to frustum-cull.
//!
//! Tiles duplicate the vertices along their shared edges (each tile owns the
//! full ring of vertices around its cells) but never duplicate a *cell*, so the
//! surface is covered exactly once. Normals come from the height field rather
//! than from face averaging, so the duplicated edge vertices get identical
//! normals and the seams are invisible.

use glam::Vec3;

use crate::mesh::Solid;

/// Vertices per tile. 512×512 cells worth — 8 MB of attributes, comfortably
/// inside every backend's buffer limit, and a chunk of work short enough not to
/// be noticed when it lands on the main thread.
pub const DEFAULT_MAX_TILE_VERTS: usize = 1 << 18;

/// A rectangular block of the surface grid, inclusive in both axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl TileRect {
    pub fn width(&self) -> usize {
        self.x1 - self.x0 + 1
    }

    pub fn height(&self) -> usize {
        self.y1 - self.y0 + 1
    }

    pub fn vertices(&self) -> usize {
        self.width() * self.height()
    }

    pub fn triangles(&self) -> usize {
        2 * (self.width() - 1) * (self.height() - 1)
    }
}

/// Split a `w × h` vertex grid into tiles of at most `max_verts` vertices.
///
/// Square-ish tiles, so no tile is a long thin strip that touches many cache
/// lines for few triangles.
pub fn plan(w: usize, h: usize, max_verts: usize) -> Vec<TileRect> {
    let w = w.max(2);
    let h = h.max(2);
    // A tile spanning `side` cells owns `side + 1` vertices per axis.
    let side = ((max_verts as f64).sqrt().floor() as usize)
        .saturating_sub(1)
        .max(1);

    let mut tiles = Vec::new();
    let mut y0 = 0;
    while y0 < h - 1 {
        let y1 = (y0 + side).min(h - 1);
        let mut x0 = 0;
        while x0 < w - 1 {
            let x1 = (x0 + side).min(w - 1);
            tiles.push(TileRect { x0, y0, x1, y1 });
            x0 = x1;
        }
        y0 = y1;
    }
    tiles
}

/// What a tile is part of, so the app can give the two different materials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileKind {
    /// The relief itself: smooth-shaded, textured with the photo if there is one.
    Surface,
    /// Skirt, bezel, wall and back plate: flat-shaded, always plain.
    Frame,
}

/// A ready-to-upload mesh chunk. Attribute layout matches Bevy's expectations,
/// so the app inserts these straight into a `Mesh` with no conversion.
#[derive(Debug, Clone)]
pub struct TileMesh {
    pub kind: TileKind,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
}

impl TileMesh {
    pub fn triangles(&self) -> usize {
        self.indices.len() / 3
    }
}

/// Analytic normal from the height field's gradient.
///
/// Central differences, clamped at the border. Both the height and the spacing
/// are millimetres, so the gradient is dimensionless and needs no rescaling.
fn normal_at(solid: &Solid, i: usize, j: usize) -> [f32; 3] {
    let field = solid.field();
    let pitch = solid.layout.pitch_mm;
    let i = i as isize;
    let j = j as isize;
    let dzdx = (field.at_clamped(i + 1, j) - field.at_clamped(i - 1, j)) / (2.0 * pitch);
    // y decreases as j increases, so the difference runs the other way.
    let dzdy = (field.at_clamped(i, j - 1) - field.at_clamped(i, j + 1)) / (2.0 * pitch);
    Vec3::new(-dzdx, -dzdy, 1.0).normalize().to_array()
}

/// Build one tile of the relief surface.
pub fn surface_tile(solid: &Solid, rect: &TileRect) -> TileMesh {
    let l = &solid.layout;
    let (tw, th) = (rect.width(), rect.height());
    let mut positions = Vec::with_capacity(tw * th);
    let mut normals = Vec::with_capacity(tw * th);
    let mut uvs = Vec::with_capacity(tw * th);

    let (du, dv) = ((l.w - 1) as f32, (l.h - 1) as f32);
    for j in rect.y0..=rect.y1 {
        for i in rect.x0..=rect.x1 {
            positions.push(solid.position(j * l.w + i).to_array());
            normals.push(normal_at(solid, i, j));
            uvs.push([i as f32 / du, j as f32 / dv]);
        }
    }

    let mut indices = Vec::with_capacity(rect.triangles() * 3);
    for j in 0..th - 1 {
        for i in 0..tw - 1 {
            let tl = (j * tw + i) as u32;
            let tr = tl + 1;
            let bl = tl + tw as u32;
            let br = bl + 1;
            indices.extend_from_slice(&[tl, bl, br, tl, br, tr]);
        }
    }

    TileMesh {
        kind: TileKind::Surface,
        positions,
        normals,
        uvs,
        indices,
    }
}

/// Build the frame as one flat-shaded mesh.
///
/// Flat rather than smooth: a bezel with rounded-looking edges reads as a
/// modelling mistake. Frame triangles are few — `7P`, some thousands even at
/// 4096 px — so the three-vertices-per-triangle expansion costs nothing.
pub fn frame_tile(solid: &Solid) -> TileMesh {
    let count = 7 * solid.layout.perimeter();
    let mut positions = Vec::with_capacity(count * 3);
    let mut normals = Vec::with_capacity(count * 3);

    solid.for_each_frame_triangle(
        |[a, b, c]| {
            let p = [
                solid.position(a as usize),
                solid.position(b as usize),
                solid.position(c as usize),
            ];
            let n = (p[1] - p[0])
                .cross(p[2] - p[0])
                .try_normalize()
                .unwrap_or(Vec3::Z)
                .to_array();
            for v in p {
                positions.push(v.to_array());
                normals.push(n);
            }
        },
        |_| {},
    );

    let indices = (0..positions.len() as u32).collect();
    let uvs = vec![[0.0, 0.0]; positions.len()];
    TileMesh {
        kind: TileKind::Frame,
        positions,
        normals,
        uvs,
        indices,
    }
}

/// Every chunk of the preview: the surface tiles, then the frame.
pub fn preview(solid: &Solid, max_verts: usize) -> Vec<TileMesh> {
    let mut out = if solid.decimation().is_some() {
        simplified_chunks(solid, max_verts)
    } else {
        plan(solid.layout.w, solid.layout.h, max_verts)
            .iter()
            .map(|rect| surface_tile(solid, rect))
            .collect()
    };
    out.push(frame_tile(solid));
    out
}

/// Chunk a simplified surface.
///
/// A simplified triangulation has no rectangular structure to cut along, so the
/// chunks are simply consecutive runs of triangles, and their vertices are not
/// shared. That costs three vertices per triangle instead of roughly one, which
/// is a poor trade in general and a fine one here: the preview is capped well
/// below export resolution, and simplification has already removed most of the
/// triangles it would apply to.
fn simplified_chunks(solid: &Solid, max_verts: usize) -> Vec<TileMesh> {
    let l = &solid.layout;
    let (du, dv) = ((l.w - 1) as f32, (l.h - 1) as f32);
    let mut chunks = Vec::new();
    let mut current = TileMesh {
        kind: TileKind::Surface,
        positions: Vec::new(),
        normals: Vec::new(),
        uvs: Vec::new(),
        indices: Vec::new(),
    };

    solid.for_each_surface_triangle(
        |triangle| {
            if current.positions.len() + 3 > max_verts {
                current.indices = (0..current.positions.len() as u32).collect();
                chunks.push(std::mem::replace(
                    &mut current,
                    TileMesh {
                        kind: TileKind::Surface,
                        positions: Vec::new(),
                        normals: Vec::new(),
                        uvs: Vec::new(),
                        indices: Vec::new(),
                    },
                ));
            }
            for vertex in triangle {
                let (i, j) = match solid.block(vertex as usize) {
                    crate::mesh::Block::Surface { i, j } => (i, j),
                    // for_each_surface_triangle only ever emits surface vertices
                    _ => unreachable!("a surface triangle left the surface"),
                };
                current
                    .positions
                    .push(solid.position(vertex as usize).to_array());
                current.normals.push(normal_at(solid, i, j));
                current.uvs.push([i as f32 / du, j as f32 / dv]);
            }
        },
        |_| {},
    );

    if !current.positions.is_empty() {
        current.indices = (0..current.positions.len() as u32).collect();
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::{BitDepth, HeightField};
    use crate::params::Params;

    fn field(w: usize, h: usize) -> HeightField {
        HeightField {
            w,
            h,
            z: (0..w * h)
                .map(|k| -10.0 * ((k % w) as f32 / w as f32))
                .collect(),
            bits: BitDepth::Sixteen,
        }
    }

    #[test]
    fn tiles_cover_every_cell_exactly_once() {
        for (w, h, max) in [(9, 5, 16), (33, 17, 64), (100, 7, 1024), (2, 2, 4)] {
            let tiles = plan(w, h, max);
            assert!(!tiles.is_empty());
            let mut covered = vec![0u8; (w - 1) * (h - 1)];
            for t in &tiles {
                assert!(t.vertices() <= max.max(4), "{t:?} exceeds {max} vertices");
                for j in t.y0..t.y1 {
                    for i in t.x0..t.x1 {
                        covered[j * (w - 1) + i] += 1;
                    }
                }
            }
            assert!(
                covered.iter().all(|&c| c == 1),
                "{w}×{h} max {max}: cells covered {:?}",
                covered.iter().collect::<std::collections::HashSet<_>>()
            );
        }
    }

    #[test]
    fn chunking_preserves_the_triangle_count() {
        let f = field(65, 33);
        let p = Params::default();
        let solid = Solid::new(&f, &p);
        let chunks = preview(&solid, 256);
        let total: usize = chunks.iter().map(|c| c.triangles()).sum();
        assert_eq!(total, solid.triangle_count());
        assert!(
            chunks.len() > 4,
            "expected several tiles, got {}",
            chunks.len()
        );
    }

    #[test]
    fn seam_vertices_agree_between_neighbouring_tiles() {
        let f = field(17, 9);
        let p = Params::default();
        let solid = Solid::new(&f, &p);
        let tiles = plan(17, 9, 36);
        // Every vertex shared by two tiles must have identical position and
        // normal, or the seam shows as a crease.
        let mut seen: std::collections::HashMap<(usize, usize), ([f32; 3], [f32; 3])> =
            Default::default();
        for rect in &tiles {
            let tile = surface_tile(&solid, rect);
            for (k, (i, j)) in (rect.y0..=rect.y1)
                .flat_map(|j| (rect.x0..=rect.x1).map(move |i| (i, j)))
                .enumerate()
            {
                let entry = (tile.positions[k], tile.normals[k]);
                if let Some(prior) = seen.insert((i, j), entry) {
                    assert_eq!(prior, entry, "seam mismatch at {i},{j}");
                }
            }
        }
    }
}
