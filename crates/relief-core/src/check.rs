//! Is it actually a solid?
//!
//! [`mesh`](crate::mesh) claims the relief comes out closed and consistently
//! wound by construction. This module is what makes that a checked claim rather
//! than a comment: [`inspect`] builds the full directed-edge map and reports
//! boundary edges, non-manifold edges and the Euler characteristic, and the test
//! suite runs it across a spread of grid sizes.
//!
//! It is deliberately not what the app calls on every rebuild. The edge map is
//! `3F` entries — 4.7 M at 1024 px, hundreds of megabytes in a hash map — so the
//! app shows [`summary`] instead, which streams and allocates nothing.

use std::collections::HashMap;

use glam::Vec3;

use crate::mesh::Solid;

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub vertices: usize,
    pub triangles: usize,
    /// Undirected edge count. `None` when the mesh was not walked.
    pub edges: Option<usize>,
    /// `V - E + F`. 2 for a closed surface of genus 0.
    pub euler: Option<i64>,
    /// Edges used by only one triangle: a hole.
    pub boundary_edges: Option<usize>,
    /// Directed edges used more than once: two faces wound the same way across
    /// one edge, which is what a slicer reports as "inverted normals".
    pub non_manifold_edges: Option<usize>,
    /// Signed volume. Positive means the winding faces outwards throughout;
    /// negative means the solid is inside out.
    pub volume_mm3: f64,
    pub min_mm: Vec3,
    pub max_mm: Vec3,
}

impl Report {
    /// Whether this mesh is fit to hand a slicer.
    pub fn watertight(&self) -> Option<bool> {
        Some(
            self.boundary_edges? == 0
                && self.non_manifold_edges? == 0
                && self.euler? == 2
                && self.volume_mm3 > 0.0,
        )
    }
}

/// Counts, bounds and volume, without walking the edges.
///
/// Everything here streams: one pass over the triangles, no allocation.
pub fn summary(solid: &Solid) -> Report {
    let (volume, min_mm, max_mm) = sweep(solid);
    Report {
        vertices: solid.vertex_count(),
        triangles: solid.triangle_count(),
        edges: None,
        euler: None,
        boundary_edges: None,
        non_manifold_edges: None,
        volume_mm3: volume,
        min_mm,
        max_mm,
    }
}

/// The full check: closure, orientation and genus.
pub fn inspect(solid: &Solid) -> Report {
    let mut directed: HashMap<(u32, u32), u32> = HashMap::with_capacity(solid.triangle_count() * 3);
    solid.for_each_triangle(
        |[a, b, c]| {
            for edge in [(a, b), (b, c), (c, a)] {
                *directed.entry(edge).or_insert(0) += 1;
            }
        },
        |_| {},
    );

    let mut boundary = 0usize;
    let mut non_manifold = 0usize;
    for (&(u, v), &count) in &directed {
        if count > 1 {
            non_manifold += 1;
        }
        if !directed.contains_key(&(v, u)) {
            boundary += 1;
        }
    }
    // Every interior edge contributes both directions.
    let edges = (directed.len() + boundary) / 2;

    let mut report = summary(solid);
    let v = report.vertices as i64;
    let f = report.triangles as i64;
    report.edges = Some(edges);
    report.euler = Some(v - edges as i64 + f);
    report.boundary_edges = Some(boundary);
    report.non_manifold_edges = Some(non_manifold);
    report
}

/// Signed volume and bounding box in one pass.
///
/// The divergence-theorem sum: for a closed surface, `Σ a · (b × c) / 6` is the
/// enclosed volume, and its sign tells you which way the normals face. It costs
/// one pass and no memory, which is why the app can afford it on every rebuild
/// and shows a filament estimate from it.
fn sweep(solid: &Solid) -> (f64, Vec3, Vec3) {
    let mut volume = 0.0f64;
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    solid.for_each_triangle(
        |[ia, ib, ic]| {
            let a = solid.position(ia as usize);
            let b = solid.position(ib as usize);
            let c = solid.position(ic as usize);
            volume += a.dot(b.cross(c)) as f64 / 6.0;
            for p in [a, b, c] {
                min = min.min(p);
                max = max.max(p);
            }
        },
        |_| {},
    );
    (volume, min, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::{BitDepth, HeightField};
    use crate::params::{Orientation, Params};

    fn bumpy(w: usize, h: usize) -> HeightField {
        let mut z = vec![0.0f32; w * h];
        for j in 0..h {
            for i in 0..w {
                let u = i as f32 / (w - 1) as f32;
                let v = j as f32 / (h - 1) as f32;
                z[j * w + i] = -30.0 * (0.5 + 0.5 * ((u * 9.0).sin() * (v * 7.0).cos()));
            }
        }
        HeightField {
            w,
            h,
            z,
            bits: BitDepth::Sixteen,
        }
    }

    /// The load-bearing test of the whole crate: whatever the grid, whatever the
    /// frame, the result is a closed, outward-wound, genus-0 solid.
    #[test]
    fn every_grid_produces_a_watertight_solid() {
        let shapes = [(2, 2), (3, 3), (9, 5), (5, 9), (16, 16), (33, 17), (7, 64)];
        // Millimetres: bezel width, bezel face height, back plate.
        let frames = [
            (5.0, 1.0, 1.0),
            (0.0, 1.0, 1.0),    // no frame at all — a different vertex layout
            (0.0, 8.0, 3.0),    // no frame, and a bezel offset that must be ignored
            (20.0, -25.0, 5.0), // frame face sunk into the relief
            (5.0, 50.0, 1.0),   // frame face standing well proud
        ];
        for (w, h) in shapes {
            for (thickness, near, back) in frames {
                let params = Params {
                    emboss_mm: 30.0,
                    frame_thickness_mm: thickness,
                    frame_near_mm: near,
                    frame_back_mm: back,
                    ..Params::default()
                };
                let field = bumpy(w, h);
                let solid = Solid::new(&field, &params);
                let report = inspect(&solid);
                assert_eq!(
                    report.boundary_edges,
                    Some(0),
                    "{w}×{h} frame {thickness}/{near}/{back}: open edges"
                );
                assert_eq!(
                    report.non_manifold_edges,
                    Some(0),
                    "{w}×{h} frame {thickness}/{near}/{back}: inconsistent winding"
                );
                assert_eq!(
                    report.euler,
                    Some(2),
                    "{w}×{h} frame {thickness}/{near}/{back}: not genus 0"
                );
                assert!(
                    report.volume_mm3 > 0.0,
                    "{w}×{h} frame {thickness}/{near}/{back}: volume {} — inside out",
                    report.volume_mm3
                );
                assert_eq!(report.watertight(), Some(true));
            }
        }
    }

    #[test]
    fn edge_count_matches_three_halves_of_the_faces() {
        // True only for a closed triangle mesh, so this is a second, independent
        // way of saying the same thing the Euler test says.
        let field = bumpy(11, 7);
        let solid = Solid::new(&field, &Params::default());
        let report = inspect(&solid);
        assert_eq!(report.edges, Some(report.triangles * 3 / 2));
    }

    #[test]
    fn volume_is_bounded_by_the_box_that_holds_it() {
        let field = bumpy(17, 13);
        let solid = Solid::new(&field, &Params::default());
        let report = inspect(&solid);
        let e = solid.layout.extents_mm();
        let box_volume = (e.x * e.y * e.z) as f64;
        assert!(report.volume_mm3 > 0.0);
        assert!(
            report.volume_mm3 < box_volume,
            "{} exceeds its bounding box {box_volume}",
            report.volume_mm3
        );
        // The plate alone is a floor: back plate plus the deepest relief.
        assert!(report.volume_mm3 > 0.1 * box_volume);
    }

    #[test]
    fn print_orientation_does_not_change_the_volume() {
        let field = bumpy(13, 9);
        let a = Solid::new(
            &field,
            &Params {
                orientation: Orientation::FaceUp,
                ..Params::default()
            },
        );
        let b = Solid::new(
            &field,
            &Params {
                orientation: Orientation::Upright,
                ..Params::default()
            },
        );
        let (va, vb) = (summary(&a).volume_mm3, summary(&b).volume_mm3);
        assert!((va - vb).abs() / va < 1e-6, "{va} vs {vb}");
    }
}

#[cfg(test)]
mod decimated {
    use super::*;
    use crate::decimate::Decimation;
    use crate::depth::{BitDepth, HeightField};
    use crate::params::Params;

    /// A relief with flat regions, sharp steps and fine texture — the three
    /// things a simplifier treats differently.
    fn mixed(w: usize, h: usize) -> HeightField {
        let mut z = vec![0.0f32; w * h];
        for j in 0..h {
            for i in 0..w {
                let u = i as f32 / (w - 1) as f32;
                let v = j as f32 / (h - 1) as f32;
                z[j * w + i] = if u < 0.3 {
                    -1.0 // flat plateau
                } else if u < 0.35 {
                    -14.0 // a cliff
                } else {
                    -8.0 - 4.0 * ((u * 21.0).sin() * (v * 17.0).cos())
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

    /// The whole point: simplification must not cost watertightness. If the
    /// balance rule or the edge stitching were wrong, this is where the holes
    /// would show up.
    #[test]
    fn a_simplified_relief_is_still_a_closed_solid() {
        for (w, h) in [(65, 65), (129, 97), (100, 71), (33, 33)] {
            for tolerance in [0.01, 0.05, 0.2, 1.0, 5.0] {
                for frame in [5.0, 0.0] {
                    let params = Params {
                        emboss_mm: 20.0,
                        frame_thickness_mm: frame,
                        ..Params::default()
                    };
                    let field = mixed(w, h);
                    let decimation = Decimation::build(&field, tolerance).expect("on");
                    let solid = Solid::new(&field, &params).with_decimation(Some(&decimation));

                    let report = inspect(&solid);
                    let what = format!("{w}×{h} tol {tolerance} frame {frame}");
                    assert_eq!(report.boundary_edges, Some(0), "{what}: open edges");
                    assert_eq!(
                        report.non_manifold_edges,
                        Some(0),
                        "{what}: inconsistent winding"
                    );
                    assert_eq!(report.euler, Some(2), "{what}: not genus 0");
                    assert!(report.volume_mm3 > 0.0, "{what}: inside out");
                }
            }
        }
    }

    /// Simplification changes the mesh, not the object: the volume it encloses
    /// has to stay put, within the tolerance it was given.
    #[test]
    fn the_volume_survives_simplification() {
        let field = mixed(129, 129);
        let params = Params {
            emboss_mm: 20.0,
            size_mm: 100.0,
            ..Params::default()
        };
        let full = summary(&Solid::new(&field, &params)).volume_mm3;

        for tolerance in [0.05, 0.2, 1.0] {
            let decimation = Decimation::build(&field, tolerance).expect("on");
            let solid = Solid::new(&field, &params).with_decimation(Some(&decimation));
            let simplified = summary(&solid).volume_mm3;
            // A tolerance of t mm over a 90 mm square can move at most
            // t * area of material; well inside 1% for anything sensible.
            let drift = (simplified - full).abs() / full;
            assert!(
                drift < 0.01,
                "tolerance {tolerance} moved the volume by {:.2}%",
                drift * 100.0
            );
            assert!(solid.triangle_count() < 2 * 128 * 128);
        }
    }
}
