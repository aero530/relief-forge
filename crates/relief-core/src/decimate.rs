//! Simplifying the relief: a restricted quadtree over the height field.
//!
//! A depth map is mostly smooth, so most of a full-resolution relief's triangles
//! describe nothing: a clamped background is dead flat, and a broad cheek or
//! forehead is nearly so. This replaces regions that a coarser quad already
//! describes well enough with that coarser quad, and keeps the fine grid only
//! where the surface actually moves.
//!
//! # Why a quadtree and not an edge collapse
//!
//! Quadric edge collapse is better at this in general, and wrong for this crate
//! in particular: it needs the whole mesh plus adjacency in memory, which is
//! several gigabytes at 4096 px and would end the streaming export, and its
//! control is a triangle ratio rather than a distance, which says nothing about
//! whether the print will be right. Here the data is a regular grid, so a
//! quadtree can measure error as **vertical deviation in millimetres** — the same
//! unit as the layer height it is judged against — and can emit triangles from a
//! small pyramid without ever materialising the mesh.
//!
//! # Crack-free, and still watertight
//!
//! Two neighbouring leaves at different levels would leave a T-vertex, which is
//! a hole. Two rules prevent it:
//!
//! * **Balance.** Neighbouring leaves differ by at most one level, forced by
//!   propagating splits outwards ([`Decimation::balance`]).
//! * **Stitching.** A leaf whose neighbour is one level finer includes that
//!   edge's midpoint in its own fan, so both sides agree on the vertices along
//!   the shared edge.
//!
//! The relief's border is decimated too, and the frame is then built on whatever
//! border survives — [`Solid`](crate::mesh::Solid) asks this module for the
//! border cycle instead of computing it arithmetically. The skirt therefore still
//! *shares* the border's indices, which is what keeps the solid watertight by
//! construction. `check::inspect` asserts it across tolerances.
//!
//! # Shaped reliefs
//!
//! When the relief is cut to an outline, [`Decimation::build_masked`] takes the
//! cells that lie wholly inside it. Any block containing a cell outside that
//! set is forced to subdivide — the same treatment a block hanging over the
//! grid's edge gets — so the boundary of the simplified surface is a staircase
//! of single cells, and an outside cell simply emits nothing.
//! [`crate::shape`] then stitches that staircase to the true outline.
//!
//! # A tolerance below the source's step size buys nothing
//!
//! An 8-bit depth map over 20 mm of relief quantises to 0.078 mm steps. Ask for
//! 0.05 mm and every one of those steps is a feature that has to be preserved:
//! measured on the sample, 61% of the triangles survive. Ask for 0.10 mm, just
//! above the step, and 33% do. [`suggested_tolerance`] is the floor that follows
//! from that, and the reason `Params::presmooth` is as much a simplification
//! control as a quality one.

use crate::depth::HeightField;

/// Smallest tolerance worth asking for, given what the source can express.
///
/// Below the quantisation step of the depth map there is nothing to gain: every
/// step becomes a feature the simplifier has to keep. Half a layer height is the
/// other floor — finer than that is invisible in the print.
pub fn suggested_tolerance(step_mm: f32, layer_mm: f32) -> f32 {
    (layer_mm * 0.5).max(step_mm)
}

/// Which of a leaf's four edges are shared with a finer neighbour, in the order
/// left, bottom, right, top.
type Stitches = [bool; 4];

/// One square of the simplified surface.
#[derive(Debug, Clone, Copy)]
pub struct Leaf {
    /// Top-left corner, in grid samples.
    pub x0: usize,
    pub y0: usize,
    /// Side length in cells: 1, 2, 4, …
    pub span: usize,
    /// Edges whose neighbour is one level finer and needs a midpoint.
    pub stitches: Stitches,
}

impl Leaf {
    /// Triangles this leaf emits: two for a single cell, otherwise a fan from
    /// its centre through four corners plus each stitched midpoint.
    pub fn triangles(&self) -> usize {
        if self.span == 1 {
            2
        } else {
            4 + self.stitches.iter().filter(|s| **s).count()
        }
    }
}

/// A simplified triangulation of a height field.
///
/// Holds no triangles: the traversal is a pure function of the split map, so
/// [`Decimation::for_each_leaf`] regenerates it on demand and the exporters stay
/// streaming.
pub struct Decimation {
    /// Grid dimensions this was built for.
    w: usize,
    h: usize,
    /// Padded cell extent, a power of two.
    pad: usize,
    /// `split[level][node]`: this node is subdivided. Level 0 is never split.
    split: Vec<Vec<bool>>,
    /// Grid index of each surviving vertex, ascending. Compact index → grid.
    used: Vec<u32>,
    /// Word-prefix counts over `member`, for grid → compact in constant time.
    ranks: Vec<u32>,
    member: Vec<u64>,
    /// The surviving border, counter-clockwise from the top-left corner.
    /// Empty for a masked surface, whose edge is the outline's business.
    border: Vec<u32>,
    /// Cells that may be part of the surface, row-major over `(w-1) × (h-1)`.
    /// `None` means all of them.
    interior: Option<Vec<bool>>,
    triangles: usize,
    tolerance_mm: f32,
    max_error_mm: f32,
}

impl Decimation {
    /// Build a simplification that never deviates from the height field by more
    /// than `tolerance_mm`, or `None` when simplification is switched off.
    pub fn build(field: &HeightField, tolerance_mm: f32) -> Option<Self> {
        if tolerance_mm <= 0.0 || field.w < 3 || field.h < 3 {
            return None;
        }
        Some(Self::build_inner(field, tolerance_mm, None))
    }

    /// Simplify only the cells marked in `interior` (row-major over the
    /// `(w-1) × (h-1)` cells); the rest emit nothing.
    ///
    /// Always returns a decimation, even at zero tolerance, where nothing is
    /// merged: a shaped surface needs the compact vertex numbering either way,
    /// so that the grid samples outside the outline are not written to a file.
    pub fn build_masked(field: &HeightField, tolerance_mm: f32, interior: Vec<bool>) -> Self {
        assert_eq!(interior.len(), (field.w - 1) * (field.h - 1));
        Self::build_inner(field, tolerance_mm, Some(interior))
    }

    fn build_inner(field: &HeightField, tolerance_mm: f32, interior: Option<Vec<bool>>) -> Self {
        let (w, h) = (field.w, field.h);
        let (cells_w, cells_h) = (w - 1, h - 1);
        let pad = cells_w.max(cells_h).next_power_of_two();
        let levels = pad.trailing_zeros() as usize;

        let mut this = Self {
            w,
            h,
            pad,
            split: Vec::new(),
            used: Vec::new(),
            ranks: Vec::new(),
            member: vec![0; w * h / 64 + 1],
            border: Vec::new(),
            interior,
            triangles: 0,
            tolerance_mm,
            max_error_mm: 0.0,
        };

        let mut errors = this.pyramid(field, levels);
        this.exclude(&mut errors, levels);
        // At zero tolerance a masked surface keeps every cell: an exactly flat
        // block would otherwise still merge, and "zero" promises the full grid.
        let tolerance = if tolerance_mm > 0.0 {
            tolerance_mm
        } else {
            -1.0
        };
        this.decide(&errors, levels, tolerance);
        this.balance(levels);
        this.collect(field);
        this
    }

    /// Is this cell part of the surface?
    pub(crate) fn cell_inside(&self, x: usize, y: usize) -> bool {
        match &self.interior {
            None => true,
            Some(mask) => mask[y * (self.w - 1) + x],
        }
    }

    /// Force every block that holds an excluded cell to subdivide.
    ///
    /// Built bottom-up as an "all inside" pyramid: a block is whole when its
    /// four children are, and a child hanging off the real grid is not.
    fn exclude(&self, errors: &mut [Vec<f32>], levels: usize) {
        let Some(mask) = &self.interior else {
            return;
        };
        let (cells_w, cells_h) = (self.w - 1, self.h - 1);
        let mut whole: Vec<bool> = (0..self.pad * self.pad)
            .map(|k| {
                let (x, y) = (k % self.pad, k / self.pad);
                x < cells_w && y < cells_h && mask[y * cells_w + x]
            })
            .collect();
        for (level, errors) in errors.iter_mut().enumerate().take(levels + 1).skip(1) {
            let blocks = self.pad >> level;
            let below = blocks * 2;
            let next: Vec<bool> = (0..blocks * blocks)
                .map(|k| {
                    let (bx, by) = (k % blocks, k / blocks);
                    [(0, 0), (1, 0), (0, 1), (1, 1)]
                        .iter()
                        .all(|&(dx, dy)| whole[(by * 2 + dy) * below + bx * 2 + dx])
                })
                .collect();
            for (error, &ok) in errors.iter_mut().zip(&next) {
                if !ok {
                    *error = f32::INFINITY;
                }
            }
            whole = next;
        }
    }

    /// Largest vertical deviation, in millimetres, between a block's samples and
    /// the bilinear surface through its four corners.
    ///
    /// Computed directly rather than accumulated up the levels: a nested bound is
    /// cheaper but only approximate, and the whole point of this control is that
    /// the number it reports is one you can trust against a layer height.
    fn pyramid(&self, field: &HeightField, levels: usize) -> Vec<Vec<f32>> {
        let (cells_w, cells_h) = (self.w - 1, self.h - 1);
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(levels + 1);
        out.push(Vec::new()); // level 0 is a single cell: exact, never consulted

        for level in 1..=levels {
            let span = 1usize << level;
            let blocks = self.pad >> level;
            let mut errors = vec![f32::INFINITY; blocks * blocks];
            crate::each_row_mut(&mut errors, blocks, |by, row| {
                let y0 = by * span;
                for (bx, slot) in row.iter_mut().enumerate() {
                    let x0 = bx * span;
                    // A block hanging over the edge of the real grid can never
                    // be a leaf; leaving it at infinity forces it to subdivide.
                    if x0 + span > cells_w || y0 + span > cells_h {
                        continue;
                    }
                    let corners = [
                        field.at(x0, y0),
                        field.at(x0 + span, y0),
                        field.at(x0, y0 + span),
                        field.at(x0 + span, y0 + span),
                    ];
                    let mut worst = 0.0f32;
                    for j in 0..=span {
                        let v = j as f32 / span as f32;
                        for i in 0..=span {
                            let u = i as f32 / span as f32;
                            let top = corners[0] + (corners[1] - corners[0]) * u;
                            let bottom = corners[2] + (corners[3] - corners[2]) * u;
                            let approximated = top + (bottom - top) * v;
                            worst = worst.max((field.at(x0 + i, y0 + j) - approximated).abs());
                        }
                    }
                    *slot = worst;
                }
            });
            out.push(errors);
        }
        out
    }

    /// First pass: a node subdivides if it cannot describe its own samples.
    fn decide(&mut self, errors: &[Vec<f32>], levels: usize, tolerance_mm: f32) {
        self.split = (0..=levels)
            .map(|level| {
                let blocks = self.pad >> level;
                if level == 0 {
                    // Single cells are the finest thing there is.
                    vec![false; blocks * blocks]
                } else {
                    errors[level]
                        .iter()
                        // Infinity marks a block that hangs over the edge of
                        // the real grid, and must subdivide whatever the
                        // tolerance says.
                        .map(|error| error.is_nan() || *error > tolerance_mm)
                        .collect()
                }
            })
            .collect();
    }

    /// Second pass: no leaf may touch a leaf more than one level coarser.
    ///
    /// A split node needs its four face-neighbours to exist at its own level,
    /// which means their parents must be split too. Walking levels from fine to
    /// coarse propagates that all the way up in a single pass.
    fn balance(&mut self, levels: usize) {
        for level in 0..levels {
            let blocks = self.pad >> level;
            let parents = blocks / 2;
            let mut promote = Vec::new();
            for by in 0..blocks {
                for bx in 0..blocks {
                    if !self.split[level][by * blocks + bx] {
                        continue;
                    }
                    // The node itself must exist, and so must its neighbours.
                    promote.push((bx / 2, by / 2));
                    for (dx, dy) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                        let nx = bx as i64 + dx;
                        let ny = by as i64 + dy;
                        if nx < 0 || ny < 0 || nx >= blocks as i64 || ny >= blocks as i64 {
                            continue;
                        }
                        promote.push((nx as usize / 2, ny as usize / 2));
                    }
                }
            }
            for (px, py) in promote {
                self.split[level + 1][py * parents + px] = true;
            }
        }
    }

    /// Walk the tree, marking the vertices that survive and counting triangles.
    fn collect(&mut self, field: &HeightField) {
        // Gathered first: marking needs the membership set mutably, and the walk
        // needs the split maps immutably.
        let mut leaves = Vec::new();
        self.for_each_leaf(|leaf| leaves.push(leaf));

        let (w, h) = (self.w, self.h);
        let mut used: Vec<u32> = Vec::new();
        let mut triangles = 0usize;
        let mut worst = 0.0f32;

        {
            let member = &mut self.member;
            let mut mark = |x: usize, y: usize, used: &mut Vec<u32>| {
                let index = (y * w + x) as u32;
                let (word, bit) = ((index / 64) as usize, index % 64);
                if member[word] & (1 << bit) == 0 {
                    member[word] |= 1 << bit;
                    used.push(index);
                }
            };

            for leaf in &leaves {
                triangles += leaf.triangles();
                let (x0, y0, span) = (leaf.x0, leaf.y0, leaf.span);
                mark(x0, y0, &mut used);
                mark(x0 + span, y0, &mut used);
                mark(x0, y0 + span, &mut used);
                mark(x0 + span, y0 + span, &mut used);
                if span > 1 {
                    let half = span / 2;
                    mark(x0 + half, y0 + half, &mut used);
                    for (edge, (mx, my)) in [
                        (0, (x0, y0 + half)),
                        (1, (x0 + half, y0 + span)),
                        (2, (x0 + span, y0 + half)),
                        (3, (x0 + half, y0)),
                    ] {
                        if leaf.stitches[edge] {
                            mark(mx, my, &mut used);
                        }
                    }
                }
                // The achieved bound: how far this leaf's quad really strays.
                worst = worst.max(deviation(field, x0, y0, span));
            }
        }

        used.sort_unstable();
        self.used = used;
        self.triangles = triangles;
        self.max_error_mm = worst;

        // Rank directory: how many vertices precede each 64-sample word.
        self.ranks = Vec::with_capacity(self.member.len());
        let mut running = 0u32;
        for word in &self.member {
            self.ranks.push(running);
            running += word.count_ones();
        }

        if self.interior.is_some() {
            return;
        }

        // The border, in the same counter-clockwise order the frame expects:
        // down the left column, right along the bottom, up the right, back along
        // the top. Whatever survived is what the frame is built on.
        let mut border = Vec::new();
        for y in 0..h {
            let index = (y * w) as u32;
            if self.contains(index) {
                border.push(index);
            }
        }
        for x in 1..w {
            let index = ((h - 1) * w + x) as u32;
            if self.contains(index) {
                border.push(index);
            }
        }
        for y in (0..h - 1).rev() {
            let index = (y * w + w - 1) as u32;
            if self.contains(index) {
                border.push(index);
            }
        }
        for x in (1..w - 1).rev() {
            let index = x as u32;
            if self.contains(index) {
                border.push(index);
            }
        }
        self.border = border;
    }

    fn contains(&self, index: u32) -> bool {
        let (word, bit) = ((index / 64) as usize, index % 64);
        self.member[word] & (1 << bit) != 0
    }

    /// Grid index → position in the surviving vertex list.
    pub fn compact(&self, index: u32) -> u32 {
        let (word, bit) = ((index / 64) as usize, index % 64);
        let mask = if bit == 0 { 0 } else { !0u64 >> (64 - bit) };
        self.ranks[word] + (self.member[word] & mask).count_ones()
    }

    /// Position in the surviving vertex list → grid index.
    pub fn grid(&self, compact: u32) -> u32 {
        self.used[compact as usize]
    }

    pub fn vertices(&self) -> usize {
        self.used.len()
    }

    pub fn triangles(&self) -> usize {
        self.triangles
    }

    pub fn border(&self) -> &[u32] {
        &self.border
    }

    pub fn tolerance_mm(&self) -> f32 {
        self.tolerance_mm
    }

    /// The deviation actually achieved, which is at most the tolerance asked for.
    pub fn max_error_mm(&self) -> f32 {
        self.max_error_mm
    }

    /// Visit every leaf of the simplified surface, in a fixed order.
    pub fn for_each_leaf(&self, mut visit: impl FnMut(Leaf)) {
        let levels = self.split.len() - 1;
        self.descend(levels, 0, 0, &mut visit);
    }

    fn descend(&self, level: usize, bx: usize, by: usize, visit: &mut impl FnMut(Leaf)) {
        let blocks = self.pad >> level;
        let span = 1usize << level;
        let (x0, y0) = (bx * span, by * span);
        // Blocks entirely outside the real grid contribute nothing.
        if x0 >= self.w - 1 || y0 >= self.h - 1 {
            return;
        }
        if level > 0 && self.split[level][by * blocks + bx] {
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                self.descend(level - 1, bx * 2 + dx, by * 2 + dy, visit);
            }
            return;
        }
        // Outside a shaped surface's outline: nothing here.
        if level == 0 && !self.cell_inside(x0, y0) {
            return;
        }
        // A leaf stitches to any neighbour that is one level finer.
        let finer = |nx: i64, ny: i64| {
            if nx < 0 || ny < 0 || nx >= blocks as i64 || ny >= blocks as i64 {
                return false;
            }
            self.split[level][ny as usize * blocks + nx as usize]
        };
        let (bx, by) = (bx as i64, by as i64);
        visit(Leaf {
            x0,
            y0,
            span,
            stitches: [
                finer(bx - 1, by),
                finer(bx, by + 1),
                finer(bx + 1, by),
                finer(bx, by - 1),
            ],
        });
    }

    /// Emit the simplified surface as triangles of compact vertex indices.
    ///
    /// Wound like the full-resolution grid: normals face the viewer.
    pub fn for_each_triangle(&self, mut emit: impl FnMut([u32; 3]), mut progress: impl FnMut(f32)) {
        let w = self.w;
        let at = |x: usize, y: usize| self.compact((y * w + x) as u32);
        let mut done = 0usize;
        let total = self.triangles.max(1);

        self.for_each_leaf(|leaf| {
            let (x0, y0, span) = (leaf.x0, leaf.y0, leaf.span);
            if span == 1 {
                let (tl, tr) = (at(x0, y0), at(x0 + 1, y0));
                let (bl, br) = (at(x0, y0 + 1), at(x0 + 1, y0 + 1));
                emit([tl, bl, br]);
                emit([tl, br, tr]);
                done += 2;
            } else {
                let half = span / 2;
                // Counter-clockwise seen from the front, matching the border
                // cycle: down the left, right along the bottom, up the right,
                // back along the top, with midpoints where a finer neighbour
                // has already placed one.
                let mut ring = [0u32; 8];
                let mut n = 0;
                let mut push = |v: u32| {
                    ring[n] = v;
                    n += 1;
                };
                push(at(x0, y0));
                if leaf.stitches[0] {
                    push(at(x0, y0 + half));
                }
                push(at(x0, y0 + span));
                if leaf.stitches[1] {
                    push(at(x0 + half, y0 + span));
                }
                push(at(x0 + span, y0 + span));
                if leaf.stitches[2] {
                    push(at(x0 + span, y0 + half));
                }
                push(at(x0 + span, y0));
                if leaf.stitches[3] {
                    push(at(x0 + half, y0));
                }

                let centre = at(x0 + half, y0 + half);
                for k in 0..n {
                    emit([centre, ring[k], ring[(k + 1) % n]]);
                }
                done += n;
            }
            progress(done as f32 / total as f32);
        });
        progress(1.0);
    }
}

/// Largest vertical deviation of a block's samples from the quad through its
/// corners — the achieved error for one leaf.
fn deviation(field: &HeightField, x0: usize, y0: usize, span: usize) -> f32 {
    if span == 1 {
        return 0.0;
    }
    let corners = [
        field.at(x0, y0),
        field.at(x0 + span, y0),
        field.at(x0, y0 + span),
        field.at(x0 + span, y0 + span),
    ];
    let mut worst = 0.0f32;
    for j in 0..=span {
        let v = j as f32 / span as f32;
        for i in 0..=span {
            let u = i as f32 / span as f32;
            let top = corners[0] + (corners[1] - corners[0]) * u;
            let bottom = corners[2] + (corners[3] - corners[2]) * u;
            worst = worst.max((field.at(x0 + i, y0 + j) - (top + (bottom - top) * v)).abs());
        }
    }
    worst
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::BitDepth;

    /// A field that is flat in one half and busy in the other, so simplification
    /// has something obvious to do and something it must not do.
    fn half_and_half(w: usize, h: usize) -> HeightField {
        let mut z = vec![0.0f32; w * h];
        for j in 0..h {
            for i in 0..w {
                z[j * w + i] = if i < w / 2 {
                    -5.0
                } else {
                    -5.0 - 3.0 * ((i as f32 * 0.7).sin() * (j as f32 * 0.9).cos())
                };
            }
        }
        HeightField {
            w,
            h,
            z,
            bits: BitDepth::Sixteen,
        }
    }

    fn flat(w: usize, h: usize) -> HeightField {
        HeightField {
            w,
            h,
            z: vec![-2.0; w * h],
            bits: BitDepth::Sixteen,
        }
    }

    /// A disc of interior cells in a `w × h` grid.
    fn disc(w: usize, h: usize) -> Vec<bool> {
        let (cw, ch) = (w - 1, h - 1);
        let (cx, cy, r) = (cw as f32 / 2.0, ch as f32 / 2.0, cw.min(ch) as f32 * 0.4);
        (0..cw * ch)
            .map(|k| {
                let (x, y) = ((k % cw) as f32 + 0.5, (k / cw) as f32 + 0.5);
                (x - cx).powi(2) + (y - cy).powi(2) < r * r
            })
            .collect()
    }

    #[test]
    fn a_mask_covers_exactly_its_own_cells() {
        for tolerance in [0.0, 0.01, 0.5, 5.0] {
            let field = half_and_half(37, 29);
            let mask = disc(37, 29);
            let d = Decimation::build_masked(&field, tolerance, mask.clone());
            let mut covered = vec![0u8; 36 * 28];
            d.for_each_leaf(|leaf| {
                for y in leaf.y0..leaf.y0 + leaf.span {
                    for x in leaf.x0..leaf.x0 + leaf.span {
                        covered[y * 36 + x] += 1;
                    }
                }
            });
            for (k, (&c, &m)) in covered.iter().zip(&mask).enumerate() {
                assert_eq!(c, u8::from(m), "cell {k} at tolerance {tolerance}");
            }
            assert!(d.border().is_empty());
        }
    }

    #[test]
    fn a_masked_flat_field_still_simplifies_inside() {
        let field = flat(65, 65);
        let mask = disc(65, 65);
        let cells = mask.iter().filter(|m| **m).count();
        let d = Decimation::build_masked(&field, 0.1, mask);
        assert!(d.triangles() < cells, "{} of {}", d.triangles(), 2 * cells);
        let full = Decimation::build_masked(&field, 0.0, disc(65, 65));
        assert_eq!(
            full.triangles(),
            2 * cells,
            "zero tolerance keeps every cell"
        );
    }

    #[test]
    fn switched_off_by_a_zero_tolerance() {
        assert!(Decimation::build(&flat(33, 33), 0.0).is_none());
    }

    #[test]
    fn a_flat_field_collapses_to_almost_nothing() {
        let field = flat(129, 129);
        let d = Decimation::build(&field, 0.05).expect("on");
        assert!(
            d.triangles() <= 8,
            "a plane should not need {} triangles",
            d.triangles()
        );
        assert_eq!(d.max_error_mm(), 0.0);
        // Its border is the four corners, and the frame will be built on those.
        assert_eq!(d.border().len(), 4);
    }

    #[test]
    fn detail_is_kept_and_flatness_is_not() {
        let field = half_and_half(129, 129);
        let d = Decimation::build(&field, 0.05).expect("on");
        // The busy half oscillates every few cells with 3 mm of amplitude, so it
        // is incompressible at this tolerance and *should* survive intact. Half
        // the field vanishing is therefore the right answer, not a weak one.
        let full = 2 * 128 * 128;
        let kept = d.triangles() as f32 / full as f32;
        assert!(
            (0.45..0.58).contains(&kept),
            "kept {:.0}% — expected the flat half to go and the busy half to stay",
            kept * 100.0
        );

        // Every leaf on the flat side is coarse; the busy side keeps fine ones.
        let mut coarse_left = 0;
        let mut fine_right = 0;
        d.for_each_leaf(|leaf| {
            if leaf.x0 + leaf.span <= 60 && leaf.span >= 16 {
                coarse_left += 1;
            }
            if leaf.x0 >= 70 && leaf.span <= 2 {
                fine_right += 1;
            }
        });
        assert!(coarse_left > 0, "the flat half did not simplify");
        assert!(fine_right > 0, "the busy half lost its detail");
    }

    #[test]
    fn the_error_bound_is_respected() {
        for tolerance in [0.01, 0.05, 0.2, 1.0] {
            let field = half_and_half(129, 97);
            let d = Decimation::build(&field, tolerance).expect("on");
            assert!(
                d.max_error_mm() <= tolerance + 1e-6,
                "asked for {tolerance}, achieved {}",
                d.max_error_mm()
            );
        }
    }

    #[test]
    fn a_looser_tolerance_never_costs_more_triangles() {
        let field = half_and_half(129, 129);
        let mut previous = usize::MAX;
        for tolerance in [0.01, 0.05, 0.1, 0.5, 2.0] {
            let d = Decimation::build(&field, tolerance).expect("on");
            assert!(
                d.triangles() <= previous,
                "{tolerance} mm produced more triangles than the tolerance before it"
            );
            previous = d.triangles();
        }
    }

    /// Balance is what keeps the stitching rule sufficient: without it a leaf
    /// could touch one two levels finer, and one midpoint would not be enough.
    #[test]
    fn neighbouring_leaves_differ_by_at_most_one_level() {
        let field = half_and_half(129, 129);
        let d = Decimation::build(&field, 0.05).expect("on");

        // Map every cell to the span of the leaf covering it.
        let mut spans = vec![0usize; 128 * 128];
        d.for_each_leaf(|leaf| {
            for y in leaf.y0..leaf.y0 + leaf.span {
                for x in leaf.x0..leaf.x0 + leaf.span {
                    spans[y * 128 + x] = leaf.span;
                }
            }
        });
        for y in 0..128 {
            for x in 0..127 {
                let (a, b) = (spans[y * 128 + x], spans[y * 128 + x + 1]);
                assert!(
                    a.max(b) <= 2 * a.min(b),
                    "spans {a} and {b} are more than one level apart at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn every_cell_is_covered_exactly_once() {
        let field = half_and_half(97, 65);
        let d = Decimation::build(&field, 0.08).expect("on");
        let mut covered = vec![0u8; 96 * 64];
        d.for_each_leaf(|leaf| {
            for y in leaf.y0..leaf.y0 + leaf.span {
                for x in leaf.x0..leaf.x0 + leaf.span {
                    covered[y * 96 + x] += 1;
                }
            }
        });
        assert!(
            covered.iter().all(|&c| c == 1),
            "leaves overlap or leave gaps"
        );
    }

    /// Grids are rarely a power of two plus one; the padded blocks that hang over
    /// the edge have to subdivide rather than invent geometry.
    #[test]
    fn awkward_grid_sizes_are_covered_exactly() {
        for (w, h) in [(100, 71), (513, 385), (37, 37), (3, 3)] {
            let field = half_and_half(w, h);
            let d = Decimation::build(&field, 0.05).expect("on");
            let mut covered = vec![0u8; (w - 1) * (h - 1)];
            d.for_each_leaf(|leaf| {
                assert!(leaf.x0 + leaf.span < w && leaf.y0 + leaf.span < h);
                for y in leaf.y0..leaf.y0 + leaf.span {
                    for x in leaf.x0..leaf.x0 + leaf.span {
                        covered[y * (w - 1) + x] += 1;
                    }
                }
            });
            assert!(
                covered.iter().all(|&c| c == 1),
                "{w}×{h} is not covered exactly once"
            );
        }
    }

    #[test]
    fn compact_indices_round_trip() {
        let field = half_and_half(65, 65);
        let d = Decimation::build(&field, 0.05).expect("on");
        assert!(d.vertices() > 4);
        for compact in 0..d.vertices() as u32 {
            assert_eq!(d.compact(d.grid(compact)), compact);
        }
    }

    #[test]
    fn the_border_is_a_walk_around_the_edge() {
        let field = half_and_half(65, 49);
        let d = Decimation::build(&field, 0.05).expect("on");
        let border = d.border();
        assert!(border.len() >= 4);
        for &index in border {
            let (x, y) = ((index as usize) % 65, (index as usize) / 65);
            assert!(
                x == 0 || y == 0 || x == 64 || y == 48,
                "{x},{y} is not on the border"
            );
            assert!(d.contains(index), "the border must survive simplification");
        }
        // No repeats, and the corners are all present.
        let unique: std::collections::HashSet<_> = border.iter().collect();
        assert_eq!(unique.len(), border.len());
        for corner in [0, 64, 48 * 65, 48 * 65 + 64] {
            assert!(border.contains(&corner), "corner {corner} missing");
        }
    }
}
