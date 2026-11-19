//! A relief cut to an outline.
//!
//! # The decomposition
//!
//! The rectangular solid is a grid plus rings built on its border cycle. A
//! shaped one keeps as much of that as it can and adds one narrow strip:
//!
//! * **Interior.** Grid cells lying wholly inside the outline, with a margin,
//!   are the surface exactly as before — simplified by the same quadtree, via
//!   [`Decimation::build_masked`]. Its boundary is a staircase of cell edges.
//! * **Collar.** The strip between that staircase and the true outline is
//!   triangulated with a constrained Delaunay triangulation whose constraints
//!   are both boundaries. It uses *only* their vertices, so it shares every
//!   staircase edge with the grid and every outline edge with the walls: no
//!   T-junctions, nothing to weld.
//! * **Walls.** Each outline loop dropped straight to the back plate — the
//!   unframed layout's sides, on a different cycle, once per loop.
//! * **Back plate.** The outline's own constrained triangulation. The
//!   rectangle's centre fan cannot be used: it is wrong for anything concave,
//!   and for anything with a hole.
//!
//! The outline is resampled so no edge is longer than a grid cell, which keeps
//! the collar's triangles cell-sized and lets the outline's heights — sampled
//! bilinearly from the field — follow the relief instead of chording across it.
//!
//! Everything here scales with the outline's length, not the grid's area, so
//! it is materialised once at build time. The interior still streams.

use std::collections::HashSet;

use spade::handles::{FixedFaceHandle, FixedVertexHandle, InnerTag};
use spade::{ConstrainedDelaunayTriangulation, Point2, Triangulation};

type Cdt = ConstrainedDelaunayTriangulation<Point2<f64>>;

use crate::decimate::Decimation;
use crate::depth::HeightField;
use crate::outline::Piece;

/// Longest outline edge, in grid cells.
const MAX_EDGE: f64 = 1.0;
/// How far a cell must stay from the outline to count as interior, in cells.
/// Keeps the collar from degenerating into slivers where the outline grazes a
/// grid corner.
const MARGIN: f64 = 0.25;

/// The parts of a shaped solid that are not the grid interior.
pub struct ShapeMesh {
    pub(crate) decimation: Decimation,
    /// Outline vertices, in grid units of the field this was built on (y down).
    ring: Vec<[f64; 2]>,
    /// Their surface height, millimetres.
    ring_z: Vec<f32>,
    /// Each loop as ring indices, wound so the material is on the left seen
    /// from the front: the outer loop counter-clockwise, holes clockwise.
    loops: Vec<Vec<u32>>,
    /// Collar triangles in the solid's index space: interior vertices by
    /// compact index, ring vertices offset by the interior's vertex count.
    collar: Vec<[u32; 3]>,
    /// Back plate triangles, as ring indices, counter-clockwise from the front.
    plate: Vec<[u32; 3]>,
    holes: usize,
    /// Grid cells inside the outline, for the unsimplified triangle count.
    interior_cells: usize,
}

/// Why an outline could not be meshed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShapeError {
    #[error("the cutout outline crosses itself")]
    SelfIntersecting,
    #[error("the cutout outline has no area")]
    Empty,
}

impl ShapeMesh {
    /// Mesh `piece` — given in grid units of `field`, y down — over `field`.
    pub fn build(
        field: &HeightField,
        piece: &Piece,
        tolerance_mm: f32,
    ) -> Result<Self, ShapeError> {
        let piece = piece.oriented().resampled(MAX_EDGE);
        if piece.loops.is_empty() || piece.loops[0].len() < 3 {
            return Err(ShapeError::Empty);
        }
        let (w, h) = (field.w, field.h);
        let (cells_w, cells_h) = (w - 1, h - 1);
        let rows = RowIndex::new(&piece, h);

        let interior = interior_cells(&piece, &rows, w, h);
        let interior_cells = interior.iter().filter(|c| **c).count();
        let decimation = Decimation::build_masked(field, tolerance_mm, interior);
        let surface = decimation.vertices() as u32;
        let inside =
            |x: usize, y: usize| x < cells_w && y < cells_h && decimation.cell_inside(x, y);

        // --- Collar: staircase and outline, constrained together. ---
        #[derive(Clone, Copy)]
        enum Ref {
            Grid(u32),
            Ring(u32),
        }
        let mut cdt: ConstrainedDelaunayTriangulation<Point2<f64>> = Default::default();
        let mut refs: Vec<Option<Ref>> = Vec::new();
        // First writer wins: a position inserted twice is one vertex.
        fn set_ref(handle: usize, r: Ref, refs: &mut Vec<Option<Ref>>) -> Ref {
            if refs.len() <= handle {
                refs.resize(handle + 1, None);
            }
            *refs[handle].get_or_insert(r)
        }

        let mut grid_handles = std::collections::HashMap::new();
        let mut grid_vertex = |x: usize,
                               y: usize,
                               cdt: &mut ConstrainedDelaunayTriangulation<Point2<f64>>,
                               refs: &mut Vec<Option<Ref>>| {
            *grid_handles.entry((x, y)).or_insert_with(|| {
                let handle = cdt
                    .insert(Point2::new(x as f64, -(y as f64)))
                    .expect("grid coordinates are finite");
                set_ref(handle.index(), Ref::Grid((y * w + x) as u32), refs);
                handle
            })
        };
        let mut staircase = Vec::new();
        for y in 0..cells_h {
            for x in 0..cells_w {
                if !inside(x, y) {
                    continue;
                }
                let sides = [
                    (x == 0 || !inside(x - 1, y), (x, y), (x, y + 1)),
                    (!inside(x + 1, y), (x + 1, y), (x + 1, y + 1)),
                    (y == 0 || !inside(x, y - 1), (x, y), (x + 1, y)),
                    (!inside(x, y + 1), (x, y + 1), (x + 1, y + 1)),
                ];
                for (open, a, b) in sides {
                    if open {
                        let ha = grid_vertex(a.0, a.1, &mut cdt, &mut refs);
                        let hb = grid_vertex(b.0, b.1, &mut cdt, &mut refs);
                        staircase.push((ha, hb));
                    }
                }
            }
        }

        let mut ring: Vec<[f64; 2]> = Vec::new();
        let mut loops: Vec<Vec<u32>> = Vec::new();
        let mut loop_handles = Vec::new();
        for l in &piece.loops {
            let mut ids: Vec<u32> = Vec::with_capacity(l.len());
            let mut handles = Vec::with_capacity(l.len());
            for &p in l {
                let handle = cdt
                    .insert(Point2::new(p[0], -p[1]))
                    .map_err(|_| ShapeError::Empty)?;
                let next = ring.len() as u32;
                let id = match set_ref(handle.index(), Ref::Ring(next), &mut refs) {
                    Ref::Ring(id) => id,
                    // The margin keeps the outline off the interior's corners.
                    Ref::Grid(_) => return Err(ShapeError::SelfIntersecting),
                };
                if id == next {
                    ring.push(p);
                }
                if ids.last() != Some(&id) {
                    ids.push(id);
                    handles.push(handle);
                }
            }
            while ids.len() > 1 && ids.first() == ids.last() {
                ids.pop();
                handles.pop();
            }
            if ids.len() >= 3 {
                loops.push(ids);
                loop_handles.push(handles);
            }
        }
        if loops.is_empty() {
            return Err(ShapeError::Empty);
        }

        // Constraint edges remembered by kind: crossing an outline edge flips
        // "inside the piece", crossing a staircase edge flips "inside the grid
        // interior". Classifying faces by walking across edges, rather than by
        // testing where each face lies, is what keeps a sliver between two
        // nearly collinear outline points on the side its neighbours put it.
        let mut stair_edges = HashSet::new();
        for (a, b) in staircase {
            for e in cdt.try_add_constraint(a, b) {
                stair_edges.insert(e.as_undirected().index());
            }
        }
        let mut outline_edges = HashSet::new();
        add_loops(&mut cdt, &loop_handles, &mut outline_edges)?;

        let to_index = |r: Ref| match r {
            Ref::Grid(g) => decimation.compact(g),
            Ref::Ring(k) => surface + k,
        };
        let mut collar = Vec::new();
        for (face, [in_piece, in_interior]) in classify(&cdt, [&outline_edges, &stair_edges]) {
            if !in_piece || in_interior {
                continue;
            }
            let r = |v: usize| refs[v].expect("every inserted vertex has a ref");
            let [a, b, c] = cdt.face(face).vertices();
            collar.push([
                to_index(r(a.fix().index())),
                to_index(r(b.fix().index())),
                to_index(r(c.fix().index())),
            ]);
        }

        // --- Back plate: the outline alone. ---
        let mut plate_cdt: ConstrainedDelaunayTriangulation<Point2<f64>> = Default::default();
        let mut by_handle: Vec<u32> = Vec::new();
        let mut plate_handles = Vec::with_capacity(ring.len());
        for (k, p) in ring.iter().enumerate() {
            let handle = plate_cdt
                .insert(Point2::new(p[0], -p[1]))
                .map_err(|_| ShapeError::Empty)?;
            if by_handle.len() <= handle.index() {
                by_handle.resize(handle.index() + 1, u32::MAX);
            }
            by_handle[handle.index()] = k as u32;
            plate_handles.push(handle);
        }
        let plate_loops: Vec<Vec<FixedVertexHandle>> = loops
            .iter()
            .map(|ids| ids.iter().map(|&k| plate_handles[k as usize]).collect())
            .collect();
        let mut plate_edges = HashSet::new();
        add_loops(&mut plate_cdt, &plate_loops, &mut plate_edges)?;
        let none = HashSet::new();
        let mut plate = Vec::new();
        for (face, [in_piece, _]) in classify(&plate_cdt, [&plate_edges, &none]) {
            if in_piece {
                let [a, b, c] = plate_cdt.face(face).vertices();
                plate.push([
                    by_handle[a.fix().index()],
                    by_handle[b.fix().index()],
                    by_handle[c.fix().index()],
                ]);
            }
        }

        let ring_z = ring.iter().map(|p| sample(field, p[0], p[1])).collect();
        Ok(Self {
            decimation,
            ring,
            ring_z,
            loops,
            collar,
            plate,
            holes: piece.holes(),
            interior_cells,
        })
    }

    pub fn decimation(&self) -> &Decimation {
        &self.decimation
    }

    /// Outline vertices: one ring on the surface, one on the back plate.
    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// Grid coordinates (y down) of outline vertex `k`.
    pub fn ring_grid(&self, k: usize) -> [f64; 2] {
        self.ring[k]
    }

    /// Surface height of outline vertex `k`, millimetres.
    pub fn ring_z(&self, k: usize) -> f32 {
        self.ring_z[k]
    }

    pub fn loops(&self) -> &[Vec<u32>] {
        &self.loops
    }

    pub fn collar(&self) -> &[[u32; 3]] {
        &self.collar
    }

    pub fn plate(&self) -> &[[u32; 3]] {
        &self.plate
    }

    /// Wall triangles: two per outline edge.
    pub fn wall_triangles(&self) -> usize {
        2 * self.loops.iter().map(Vec::len).sum::<usize>()
    }

    /// Holes through the solid. A closed surface with `g` of them has Euler
    /// characteristic `2 - 2g`.
    pub fn holes(&self) -> usize {
        self.holes
    }

    /// Triangles the same solid would have without simplification.
    pub fn full_triangles(&self) -> usize {
        2 * self.interior_cells + self.collar.len() + self.wall_triangles() + self.plate.len()
    }
}

/// Constrain every loop edge, remembering which triangulation edges they became.
fn add_loops(
    cdt: &mut Cdt,
    loops: &[Vec<FixedVertexHandle>],
    edges: &mut HashSet<usize>,
) -> Result<(), ShapeError> {
    for handles in loops {
        for k in 0..handles.len() {
            let (a, b) = (handles[k], handles[(k + 1) % handles.len()]);
            let made = cdt.try_add_constraint(a, b);
            if made.is_empty() {
                return Err(ShapeError::SelfIntersecting);
            }
            edges.extend(made.iter().map(|e| e.as_undirected().index()));
        }
    }
    Ok(())
}

/// Every inner face with two parities: how many edges of each kind separate
/// it from the outside of the triangulation, mod 2.
fn classify(cdt: &Cdt, kinds: [&HashSet<usize>; 2]) -> Vec<(FixedFaceHandle<InnerTag>, [bool; 2])> {
    let flips = |undirected: usize| {
        [
            kinds[0].contains(&undirected),
            kinds[1].contains(&undirected),
        ]
    };
    let mut state: Vec<Option<[bool; 2]>> = vec![None; cdt.num_all_faces()];
    let mut queue = std::collections::VecDeque::new();
    for hull in cdt.convex_hull() {
        let Some(inner) = hull.rev().face().as_inner() else {
            continue;
        };
        let index = inner.fix().index();
        if state[index].is_none() {
            state[index] = Some(flips(hull.as_undirected().fix().index()));
            queue.push_back(inner.fix());
        }
    }
    let mut out = Vec::with_capacity(cdt.num_inner_faces());
    while let Some(face) = queue.pop_front() {
        let here = state[face.index()].expect("queued faces have a state");
        out.push((face, here));
        for edge in cdt.face(face).adjacent_edges() {
            let Some(next) = edge.rev().face().as_inner() else {
                continue;
            };
            let index = next.fix().index();
            if state[index].is_none() {
                let f = flips(edge.as_undirected().fix().index());
                state[index] = Some([here[0] ^ f[0], here[1] ^ f[1]]);
                queue.push_back(next.fix());
            }
        }
    }
    out
}

/// Bilinear height at fractional grid coordinates, clamped to the field.
fn sample(field: &HeightField, x: f64, y: f64) -> f32 {
    let x = x.clamp(0.0, (field.w - 1) as f64);
    let y = y.clamp(0.0, (field.h - 1) as f64);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(field.w - 1), (y0 + 1).min(field.h - 1));
    let (tx, ty) = ((x - x0 as f64) as f32, (y - y0 as f64) as f32);
    let top = field.at(x0, y0) + (field.at(x1, y0) - field.at(x0, y0)) * tx;
    let bottom = field.at(x0, y1) + (field.at(x1, y1) - field.at(x0, y1)) * tx;
    top + (bottom - top) * ty
}

/// The outline's edges bucketed by the grid row they span, so a scanline
/// costs a handful of edges rather than all of them.
struct RowIndex {
    rows: Vec<Vec<([f64; 2], [f64; 2])>>,
}

impl RowIndex {
    fn new(piece: &Piece, h: usize) -> Self {
        let mut rows = vec![Vec::new(); h.max(1)];
        let last = rows.len() as f64 - 1.0;
        for l in &piece.loops {
            for (k, &a) in l.iter().enumerate() {
                let b = l[(k + 1) % l.len()];
                let lo = a[1].min(b[1]).floor().clamp(0.0, last) as usize;
                let hi = a[1].max(b[1]).floor().clamp(0.0, last) as usize;
                for row in &mut rows[lo..=hi] {
                    row.push((a, b));
                }
            }
        }
        Self { rows }
    }

    /// Every x where the horizontal line at `y` crosses the outline, sorted.
    fn crossings(&self, y: f64) -> Vec<f64> {
        let mut xs = Vec::new();
        if y < 0.0 || y >= self.rows.len() as f64 {
            return xs;
        }
        for &(a, b) in &self.rows[y.floor() as usize] {
            if (a[1] > y) != (b[1] > y) {
                xs.push(a[0] + (y - a[1]) * (b[0] - a[0]) / (b[1] - a[1]));
            }
        }
        xs.sort_by(f64::total_cmp);
        xs
    }
}

/// Cells wholly inside the piece and at least [`MARGIN`] from its outline.
fn interior_cells(piece: &Piece, rows: &RowIndex, w: usize, h: usize) -> Vec<bool> {
    let (cells_w, cells_h) = (w - 1, h - 1);

    // Which grid vertices are inside: one scanline per row.
    let mut vertex_inside = vec![false; w * h];
    for j in 0..h {
        let xs = rows.crossings(j as f64);
        let mut k = 0;
        let mut inside = false;
        for i in 0..w {
            while k < xs.len() && xs[k] <= i as f64 {
                inside = !inside;
                k += 1;
            }
            vertex_inside[j * w + i] = inside;
        }
    }

    // Which cells the outline passes through or near.
    let mut near = vec![false; cells_w * cells_h];
    for l in &piece.loops {
        for (k, &a) in l.iter().enumerate() {
            let b = l[(k + 1) % l.len()];
            let range = |lo: f64, hi: f64, n: usize| {
                let lo = (lo - MARGIN).floor().max(0.0) as usize;
                let hi = ((hi + MARGIN).floor().max(0.0) as usize).min(n.saturating_sub(1));
                lo..=hi
            };
            for y in range(a[1].min(b[1]), a[1].max(b[1]), cells_h) {
                for x in range(a[0].min(b[0]), a[0].max(b[0]), cells_w) {
                    near[y * cells_w + x] = true;
                }
            }
        }
    }

    (0..cells_w * cells_h)
        .map(|k| {
            let (x, y) = (k % cells_w, k / cells_w);
            !near[k]
                && vertex_inside[y * w + x]
                && vertex_inside[y * w + x + 1]
                && vertex_inside[(y + 1) * w + x]
                && vertex_inside[(y + 1) * w + x + 1]
        })
        .collect()
}
