//! How the height field becomes a closed solid.
//!
//! # The solid is a view, not a buffer
//!
//! [`Solid`] borrows a [`HeightField`] and computes any vertex position on
//! demand from its index. It knows its vertex and triangle counts before it
//! emits anything, and it emits triangles in a fixed order. Together that is
//! everything the exporters need to stream a mesh of any size through a
//! constant-size buffer — nothing here ever allocates a vertex array.
//!
//! # Layout
//!
//! Everything is millimetres, and every vertex outside the surface grid belongs
//! to a ring built on the surface's border cycle, of length `P = 2(w + h) - 4`.
//!
//! With a frame:
//!
//! ```text
//!   [0 .. w*h)              surface, row-major
//!   [w*h .. w*h+P)          rim    — the data border lifted to the bezel plane
//!   [w*h+P .. w*h+2P)       outer  — the rim pushed out to the frame edge
//!   [w*h+2P .. w*h+3P)      back   — outer, dropped to the back plate
//!    w*h+3P                 centre of the back plate
//! ```
//!
//! Without one (`frame_thickness_mm == 0`) the rim and bezel are not thin, they
//! are *absent*: the relief's sides drop straight from its border to the plate.
//!
//! ```text
//!   [0 .. w*h)              surface, row-major
//!   [w*h .. w*h+P)          back   — the border, dropped to the back plate
//!    w*h+P                  centre of the back plate
//! ```
//!
//! Either way each strip *shares* the indices of the ring beside it, which is
//! why no weld pass is needed: there is no duplicate seam vertex to merge. The
//! Euler characteristic works out to 2 in both layouts, and
//! [`crate::check::inspect`] asserts it on real meshes.

use glam::Vec3;

use crate::decimate::Decimation;
use crate::depth::{Albedo, HeightField};
use crate::params::{Orientation, Params, Units};

/// Mid grey, matching upstream's uncoloured vertices.
pub const FRAME_GREY: [u8; 3] = [0x80, 0x80, 0x80];

/// Everything about the solid's shape that does not depend on a single vertex.
///
/// All lengths are millimetres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub w: usize,
    pub h: usize,
    /// Distance between neighbouring vertices.
    pub pitch_mm: f32,
    /// Half the relief's own extent, frame excluded.
    pub half_w_mm: f32,
    pub half_h_mm: f32,
    /// Bezel width outside the relief. Zero means there is no frame.
    pub thickness_mm: f32,
    /// Bezel face, already clamped against the relief depth. Zero without a frame.
    pub frame_near_mm: f32,
    /// Back plate plane (negative).
    pub frame_far_mm: f32,
    /// Relief depth.
    pub emboss_mm: f32,
    /// The unit exported coordinates are written in.
    pub units: Units,
    pub orientation: Orientation,
}

impl Layout {
    pub fn new(field: &HeightField, params: &Params) -> Self {
        let (w, h) = (field.w.max(2), field.h.max(2));

        let thickness_mm = if params.framed() {
            params.frame_thickness_mm
        } else {
            0.0
        };

        // The requested size is the longest *outer* side, so the relief itself
        // gets what the frame leaves. Deriving the scale from the face rather
        // than from the whole bounding box — which is what upstream does — keeps
        // the controls independent: changing the relief depth no longer changes
        // how wide the plaque prints.
        let data_long = (params.size_mm - 2.0 * thickness_mm).max(1.0);
        let pitch_mm = data_long / (w.max(h) - 1) as f32;

        // Upstream divides by `d_max - 1` but takes half-extents as
        // `w / (2 (d_max - 1))`, which leaves the data half a step off centre
        // inside its frame. Centring on `(w - 1) / 2` is the fix.
        let half_w_mm = (w - 1) as f32 * pitch_mm / 2.0;
        let half_h_mm = (h - 1) as f32 * pitch_mm / 2.0;

        Self {
            w,
            h,
            pitch_mm,
            half_w_mm,
            half_h_mm,
            thickness_mm,
            frame_near_mm: params.frame_near_clamped(),
            frame_far_mm: params.frame_far(),
            emboss_mm: params.emboss_mm,
            units: params.units,
            orientation: params.orientation,
        }
    }

    /// Does this solid have a bezel, or do its sides drop straight to the plate?
    pub fn framed(&self) -> bool {
        self.thickness_mm > 0.0
    }

    /// Length of the border cycle.
    pub fn perimeter(&self) -> usize {
        2 * (self.w + self.h) - 4
    }

    /// Rings between the surface and the back plate's centre: three with a frame
    /// (rim, outer, back), one without (back).
    pub(crate) fn rings(&self) -> usize {
        if self.framed() { 3 } else { 1 }
    }

    pub fn vertex_count(&self) -> usize {
        self.w * self.h + self.rings() * self.perimeter() + 1
    }

    pub fn triangle_count(&self) -> usize {
        // Two per quad, two per ring step per strip, one per back-plate fan step.
        let strips = self.rings();
        2 * (self.w - 1) * (self.h - 1) + (2 * strips + 1) * self.perimeter()
    }

    /// Grid coordinates of the `k`th border vertex.
    ///
    /// Counter-clockwise seen from the front (+Z): down the left column, right
    /// along the bottom, up the right column, left along the top. Every strip
    /// built on this cycle inherits a consistent winding from it.
    pub fn border(&self, k: usize) -> (usize, usize) {
        let (w, h) = (self.w, self.h);
        if k < h {
            (0, k)
        } else if k < h + w - 1 {
            (k - h + 1, h - 1)
        } else if k < 2 * h + w - 2 {
            (w - 1, 2 * h + w - 3 - k)
        } else {
            (2 * h + 2 * w - 4 - k, 0)
        }
    }

    /// Position of a surface grid vertex.
    fn grid_xy(&self, i: usize, j: usize) -> (f32, f32) {
        (
            (i as f32 - (self.w - 1) as f32 / 2.0) * self.pitch_mm,
            ((self.h - 1) as f32 / 2.0 - j as f32) * self.pitch_mm,
        )
    }

    /// A border vertex pushed out to the frame's outer edge.
    ///
    /// Edge vertices move perpendicular to their edge; the four corners move
    /// diagonally, which is what keeps the ring a clean quad strip with no fan.
    /// Without a frame the push is zero and this is the border itself.
    fn outer_xy(&self, i: usize, j: usize) -> (f32, f32) {
        let (x, y) = self.grid_xy(i, j);
        let x = if i == 0 {
            -(self.half_w_mm + self.thickness_mm)
        } else if i == self.w - 1 {
            self.half_w_mm + self.thickness_mm
        } else {
            x
        };
        let y = if j == 0 {
            self.half_h_mm + self.thickness_mm
        } else if j == self.h - 1 {
            -(self.half_h_mm + self.thickness_mm)
        } else {
            y
        };
        (x, y)
    }

    /// Front-most plane of the solid: the bezel face where it stands proud, the
    /// relief's own front otherwise.
    fn front_mm(&self) -> f32 {
        self.frame_near_mm.max(0.0)
    }

    /// Bounding box in millimetres, in view space.
    pub fn extents_mm(&self) -> Vec3 {
        Vec3::new(
            2.0 * (self.half_w_mm + self.thickness_mm),
            2.0 * (self.half_h_mm + self.thickness_mm),
            self.front_mm() - self.frame_far_mm,
        )
    }

    /// Relief depth — the number the step-size check divides.
    pub fn relief_mm(&self) -> f32 {
        self.emboss_mm
    }

    /// Rotate view space into the chosen print orientation and drop the result
    /// onto the bed, so an exported file needs no arranging.
    ///
    /// Millimetres in, millimetres out; [`Solid::print_position`] applies the
    /// export unit afterwards.
    pub fn to_print(&self, p: Vec3) -> Vec3 {
        match self.orientation {
            Orientation::FaceUp => Vec3::new(p.x, p.y, p.z - self.frame_far_mm),
            // +90° about X: image-up becomes +Z, relief faces -Y.
            Orientation::Upright => Vec3::new(p.x, -p.z, p.y + self.half_h_mm + self.thickness_mm),
        }
    }
}

/// Index blocks, so callers can talk about a vertex without knowing the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Surface {
        i: usize,
        j: usize,
    },
    /// The border lifted to the bezel plane. Framed solids only.
    Rim(usize),
    /// The rim pushed out to the frame's edge. Framed solids only.
    Outer(usize),
    /// The outline dropped to the back plate.
    Back(usize),
    Centre,
}

/// A closed relief solid over a borrowed height field.
pub struct Solid<'a> {
    pub layout: Layout,
    field: &'a HeightField,
    albedo: Option<&'a Albedo>,
    /// When present, the surface is the simplified triangulation rather than
    /// the full grid — and the frame is built on the border *it* leaves behind,
    /// which is what keeps the solid closed by construction either way.
    decimation: Option<&'a Decimation>,
}

impl<'a> Solid<'a> {
    pub fn new(field: &'a HeightField, params: &Params) -> Self {
        Self {
            layout: Layout::new(field, params),
            field,
            albedo: None,
            decimation: None,
        }
    }

    pub fn with_albedo(mut self, albedo: Option<&'a Albedo>) -> Self {
        self.albedo = albedo;
        self
    }

    /// Use a simplified surface. The height field must be the one it was built
    /// from, or the vertices will not line up.
    pub fn with_decimation(mut self, decimation: Option<&'a Decimation>) -> Self {
        self.decimation = decimation;
        self
    }

    pub fn decimation(&self) -> Option<&Decimation> {
        self.decimation
    }

    /// Surface vertices: every grid sample, or only the ones that survived.
    fn surface_vertices(&self) -> usize {
        match self.decimation {
            None => self.layout.w * self.layout.h,
            Some(d) => d.vertices(),
        }
    }

    fn surface_triangles(&self) -> usize {
        match self.decimation {
            None => 2 * (self.layout.w - 1) * (self.layout.h - 1),
            Some(d) => d.triangles(),
        }
    }

    /// Length of the border cycle the frame is built on.
    pub fn perimeter(&self) -> usize {
        match self.decimation {
            None => self.layout.perimeter(),
            Some(d) => d.border().len(),
        }
    }

    /// Grid coordinates of the `k`th border vertex.
    fn border_cell(&self, k: usize) -> (usize, usize) {
        match self.decimation {
            None => self.layout.border(k),
            Some(d) => {
                let grid = d.border()[k] as usize;
                (grid % self.layout.w, grid / self.layout.w)
            }
        }
    }

    /// Vertex index of a surface sample.
    fn surface_index(&self, i: usize, j: usize) -> u32 {
        let flat = (j * self.layout.w + i) as u32;
        match self.decimation {
            None => flat,
            Some(d) => d.compact(flat),
        }
    }

    pub fn field(&self) -> &HeightField {
        self.field
    }

    pub fn has_colour(&self) -> bool {
        self.albedo.is_some()
    }

    pub fn vertex_count(&self) -> usize {
        self.surface_vertices() + self.layout.rings() * self.perimeter() + 1
    }

    pub fn triangle_count(&self) -> usize {
        self.surface_triangles() + (2 * self.layout.rings() + 1) * self.perimeter()
    }

    /// Which block a flat vertex index falls in.
    pub fn block(&self, index: usize) -> Block {
        let l = &self.layout;
        let surface = self.surface_vertices();
        let p = self.perimeter();
        if index < surface {
            let grid = match self.decimation {
                None => index,
                Some(d) => d.grid(index as u32) as usize,
            };
            return Block::Surface {
                i: grid % l.w,
                j: grid / l.w,
            };
        }
        let k = index - surface;
        if !l.framed() {
            return if k < p { Block::Back(k) } else { Block::Centre };
        }
        match k / p {
            0 => Block::Rim(k),
            1 => Block::Outer(k - p),
            2 => Block::Back(k - 2 * p),
            _ => Block::Centre,
        }
    }

    /// View-space position in millimetres: x right, y up, relief towards +Z.
    pub fn position(&self, index: usize) -> Vec3 {
        let l = &self.layout;
        match self.block(index) {
            Block::Surface { i, j } => {
                let (x, y) = l.grid_xy(i, j);
                Vec3::new(x, y, self.field.at(i, j))
            }
            Block::Rim(k) => {
                let (i, j) = self.border_cell(k);
                let (x, y) = l.grid_xy(i, j);
                Vec3::new(x, y, l.frame_near_mm)
            }
            Block::Outer(k) => {
                let (i, j) = self.border_cell(k);
                let (x, y) = l.outer_xy(i, j);
                Vec3::new(x, y, l.frame_near_mm)
            }
            Block::Back(k) => {
                let (i, j) = self.border_cell(k);
                let (x, y) = l.outer_xy(i, j);
                Vec3::new(x, y, l.frame_far_mm)
            }
            Block::Centre => Vec3::new(0.0, 0.0, l.frame_far_mm),
        }
    }

    /// Position as written to a file: print orientation, resting on the bed, in
    /// the chosen export unit.
    pub fn print_position(&self, index: usize) -> Vec3 {
        self.layout.to_print(self.position(index)) * self.layout.units.per_mm()
    }

    /// Vertex colour, from the photo where there is one.
    ///
    /// Frame vertices stay grey whether or not a photo was supplied: the frame
    /// is not part of the picture, and tinting it with the border pixels reads
    /// as a smear.
    pub fn colour(&self, index: usize) -> [u8; 3] {
        match (self.block(index), self.albedo) {
            (Block::Surface { i, j }, Some(a)) => a.at(i, j),
            _ => FRAME_GREY,
        }
    }

    /// Emit every triangle as vertex indices, in the canonical order.
    ///
    /// Surface first, row by row, so a consumer that wants to write as it goes
    /// touches the height field in cache order. `progress` is called a few
    /// hundred times with a fraction in `0.0..=1.0`, which is what lets an
    /// export show a progress line.
    pub fn for_each_triangle(&self, mut emit: impl FnMut([u32; 3]), mut progress: impl FnMut(f32)) {
        self.for_each_surface_triangle(&mut emit, |t| progress(t * 0.8));
        self.for_each_frame_triangle(&mut emit, |t| progress(0.8 + t * 0.2));
    }

    /// The relief sheet alone: two triangles per pixel quad, wound so the normal
    /// points at the viewer.
    pub fn for_each_surface_triangle(
        &self,
        mut emit: impl FnMut([u32; 3]),
        mut progress: impl FnMut(f32),
    ) {
        if let Some(decimation) = self.decimation {
            decimation.for_each_triangle(emit, progress);
            return;
        }
        let (w, h) = (self.layout.w, self.layout.h);
        for j in 0..h - 1 {
            for i in 0..w - 1 {
                let tl = (j * w + i) as u32;
                let tr = tl + 1;
                let bl = tl + w as u32;
                let br = bl + 1;
                emit([tl, bl, br]);
                emit([tl, br, tr]);
            }
            progress((j + 1) as f32 / (h - 1) as f32);
        }
    }

    /// Everything that turns the sheet into a solid: with a frame, the skirt,
    /// bezel, wall and plate; without one, the sides and the plate.
    ///
    /// Separate from the surface because the preview needs them apart — the
    /// sheet is chunked and smooth-shaded, this is one small flat-shaded mesh —
    /// while an export wants them in one index space.
    pub fn for_each_frame_triangle(
        &self,
        mut emit: impl FnMut([u32; 3]),
        mut progress: impl FnMut(f32),
    ) {
        let l = &self.layout;
        let p = self.perimeter();
        let surface = self.surface_vertices();
        let framed = l.framed();

        // Every ring strip is wound the same way, and that is forced, not
        // chosen. The surface's normal faces the viewer, so its boundary runs
        // counter-clockwise — the same direction as the cycle. A strip sharing
        // that boundary has to traverse it *backwards*, or the shared edge is
        // emitted twice in the same direction and the mesh is neither closed nor
        // consistently oriented. Each strip then hands the next ring its own
        // edges in the cycle direction again, so the winding repeats all the way
        // to the back plate, whose fan reverses it one last time.
        //
        // That the normals come out facing outwards is a consequence, and worth
        // stating for the skirt because it is counter-intuitive: the skirt is a
        // step wall standing at the data edge, and the material sits on the
        // *frame* side of it, so its outward normal looks inwards.
        let pair = |a0: u32, a1: u32, b0: u32, b1: u32| [[a0, b1, a1], [a0, b0, b1]];
        let border_vertex = |k: usize| {
            let (i, j) = self.border_cell(k);
            self.surface_index(i, j)
        };

        let rim = surface as u32;
        let outer = rim + p as u32;
        let back = if framed { outer + p as u32 } else { rim };
        let centre = back + p as u32;

        if framed {
            // 1. Skirt: the data border forward to the bezel plane.
            for k in 0..p {
                let k2 = (k + 1) % p;
                for t in pair(
                    border_vertex(k),
                    border_vertex(k2),
                    rim + k as u32,
                    rim + k2 as u32,
                ) {
                    emit(t);
                }
            }
            progress(0.25);

            // 2. Bezel: flat ring from the rim out to the frame edge.
            for k in 0..p {
                let k2 = (k + 1) % p;
                for t in pair(
                    rim + k as u32,
                    rim + k2 as u32,
                    outer + k as u32,
                    outer + k2 as u32,
                ) {
                    emit(t);
                }
            }
            progress(0.5);

            // 3. Outer wall: frame edge dropping back to the plate.
            for k in 0..p {
                let k2 = (k + 1) % p;
                for t in pair(
                    outer + k as u32,
                    outer + k2 as u32,
                    back + k as u32,
                    back + k2 as u32,
                ) {
                    emit(t);
                }
            }
            progress(0.75);
        } else {
            // The relief's own sides, straight from its border to the plate.
            for k in 0..p {
                let k2 = (k + 1) % p;
                for t in pair(
                    border_vertex(k),
                    border_vertex(k2),
                    back + k as u32,
                    back + k2 as u32,
                ) {
                    emit(t);
                }
            }
            progress(0.75);
        }

        // The back plate, as a fan. Wound to face -Z.
        for k in 0..p {
            let k2 = (k + 1) % p;
            emit([centre, back + k2 as u32, back + k as u32]);
        }
        progress(1.0);
    }

    /// Emit every triangle as three print-space positions plus its normal.
    ///
    /// For STL, which stores no indices at all.
    pub fn for_each_facet(&self, mut emit: impl FnMut([Vec3; 3], Vec3), progress: impl FnMut(f32)) {
        self.for_each_triangle(
            |[a, b, c]| {
                let tri = [
                    self.print_position(a as usize),
                    self.print_position(b as usize),
                    self.print_position(c as usize),
                ];
                let normal = (tri[1] - tri[0])
                    .cross(tri[2] - tri[0])
                    .try_normalize()
                    .unwrap_or(Vec3::Z);
                emit(tri, normal);
            },
            progress,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::BitDepth;

    fn field(w: usize, h: usize) -> HeightField {
        // A dome in millimetres, so no two rows are alike and the winding tests
        // are not accidentally passing on a flat plane.
        let mut z = vec![0.0f32; w * h];
        for j in 0..h {
            for i in 0..w {
                let u = i as f32 / (w - 1) as f32 - 0.5;
                let v = j as f32 / (h - 1) as f32 - 0.5;
                z[j * w + i] = -20.0 * (u * u + v * v);
            }
        }
        HeightField {
            w,
            h,
            z,
            bits: BitDepth::Sixteen,
        }
    }

    fn params() -> Params {
        Params {
            emboss_mm: 20.0,
            size_mm: 100.0,
            frame_thickness_mm: 5.0,
            frame_near_mm: 1.0,
            frame_back_mm: 1.0,
            ..Params::default()
        }
    }

    #[test]
    fn border_cycle_is_a_closed_walk_of_unique_vertices() {
        for (w, h) in [(2, 2), (3, 4), (9, 5), (5, 9), (17, 17)] {
            let l = Layout::new(&field(w, h), &params());
            let p = l.perimeter();
            let mut seen = std::collections::HashSet::new();
            let mut previous = l.border(p - 1);
            for k in 0..p {
                let current = l.border(k);
                assert!(
                    current.0 < w && current.1 < h,
                    "{current:?} outside {w}×{h}"
                );
                assert!(seen.insert(current), "{current:?} visited twice");
                let step = (
                    current.0.abs_diff(previous.0),
                    current.1.abs_diff(previous.1),
                );
                assert!(
                    step.0 + step.1 == 1,
                    "{previous:?} -> {current:?} is not one step"
                );
                assert!(
                    current.0 == 0 || current.0 == w - 1 || current.1 == 0 || current.1 == h - 1,
                    "{current:?} is not on the border"
                );
                previous = current;
            }
            assert_eq!(seen.len(), p);
        }
    }

    #[test]
    fn counts_match_the_closed_form() {
        let p = 2 * (9 + 5) - 4;
        for (frame, rings) in [(5.0, 3), (0.0, 1)] {
            let params = Params {
                frame_thickness_mm: frame,
                ..params()
            };
            let f = field(9, 5);
            let l = Layout::new(&f, &params);
            assert_eq!(l.perimeter(), p);
            assert_eq!(l.vertex_count(), 9 * 5 + rings * p + 1);
            assert_eq!(l.triangle_count(), 2 * 8 * 4 + (2 * rings + 1) * p);

            let solid = Solid::new(&f, &params);
            let mut emitted = 0;
            solid.for_each_triangle(|_| emitted += 1, |_| {});
            assert_eq!(emitted, l.triangle_count(), "frame {frame}");
        }
    }

    #[test]
    fn the_grid_is_centred() {
        let f = field(9, 5);
        let solid = Solid::new(&f, &params());
        let l = solid.layout;
        // Left-most and right-most columns must be equal and opposite — the
        // half-pixel asymmetry upstream carries would break this.
        let left = solid.position(0).x;
        let right = solid.position(l.w - 1).x;
        assert!((left + right).abs() < 1e-4, "{left} vs {right}");
        let top = solid.position(0).y;
        let bottom = solid.position((l.h - 1) * l.w).y;
        assert!((top + bottom).abs() < 1e-4, "{top} vs {bottom}");
    }

    #[test]
    fn printed_size_is_the_longest_outer_side() {
        for (w, h) in [(9, 5), (5, 9), (7, 7)] {
            for frame in [0.0, 5.0, 20.0] {
                let params = Params {
                    size_mm: 120.0,
                    frame_thickness_mm: frame,
                    ..params()
                };
                let l = Layout::new(&field(w, h), &params);
                let e = l.extents_mm();
                let longest = e.x.max(e.y);
                assert!(
                    (longest - 120.0).abs() < 1e-3,
                    "{w}×{h} frame {frame}: longest face side {longest}"
                );
            }
        }
    }

    /// The reason the scale comes from the face and not the bounding box.
    #[test]
    fn relief_depth_does_not_change_the_printed_width() {
        let f = field(9, 5);
        let shallow = Layout::new(
            &f,
            &Params {
                emboss_mm: 2.0,
                ..params()
            },
        );
        let deep = Layout::new(
            &f,
            &Params {
                emboss_mm: 90.0,
                ..params()
            },
        );
        assert!((shallow.extents_mm().x - deep.extents_mm().x).abs() < 1e-3);
        assert!((deep.extents_mm().z - shallow.extents_mm().z - 88.0).abs() < 1e-3);
    }

    #[test]
    fn frame_never_passes_the_deepest_point() {
        let p = Params {
            emboss_mm: 30.0,
            frame_near_mm: -90.0,
            ..params()
        };
        let l = Layout::new(&field(5, 5), &p);
        assert!((l.frame_near_mm + 30.0).abs() < 1e-6);
    }

    #[test]
    fn without_a_frame_the_sides_are_flush_with_the_relief() {
        let f = field(9, 5);
        let params = Params {
            frame_thickness_mm: 0.0,
            frame_near_mm: 8.0, // ignored: there is no bezel to offset
            ..params()
        };
        let solid = Solid::new(&f, &params);
        assert!(!solid.layout.framed());
        assert_eq!(solid.layout.frame_near_mm, 0.0);

        // Nothing stands in front of the relief's own front plane.
        let front = (0..solid.vertex_count())
            .map(|v| solid.position(v).z)
            .fold(f32::NEG_INFINITY, f32::max);
        assert!(front.abs() < 1e-6, "something stands proud at {front}");

        // And the outline is the relief's own border, not a pushed-out ring.
        let border = solid.position(0);
        let back = solid.position(solid.layout.w * solid.layout.h);
        assert!((border.x - back.x).abs() < 1e-6 && (border.y - back.y).abs() < 1e-6);
    }

    #[test]
    fn print_orientation_drops_the_model_onto_the_bed() {
        let f = field(9, 5);
        for orientation in Orientation::ALL {
            for frame in [0.0, 5.0] {
                let solid = Solid::new(
                    &f,
                    &Params {
                        orientation,
                        frame_thickness_mm: frame,
                        ..params()
                    },
                );
                let mut lowest = f32::INFINITY;
                for v in 0..solid.vertex_count() {
                    lowest = lowest.min(solid.print_position(v).z);
                }
                assert!(
                    lowest.abs() < 1e-3,
                    "{orientation:?} frame {frame} leaves the model at z = {lowest}"
                );
            }
        }
    }

    #[test]
    fn export_units_scale_the_file_and_nothing_else() {
        let f = field(9, 5);
        let metric = Solid::new(&f, &params());
        let imperial = Solid::new(
            &f,
            &Params {
                units: Units::In,
                ..params()
            },
        );
        // The model is identical...
        assert_eq!(metric.position(0), imperial.position(0));
        assert_eq!(metric.layout.extents_mm(), imperial.layout.extents_mm());
        // ...only the numbers written out differ.
        let a = metric.print_position(0);
        let b = imperial.print_position(0);
        assert!((a.x / 25.4 - b.x).abs() < 1e-5, "{a} vs {b}");
    }
}
