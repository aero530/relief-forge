//! A relief cut to a drawn outline, as separate solids.
//!
//! [`Cutout`] is what loading a drawing produces once: the height data over the
//! outline's bounding box, the outline itself, and the box's size in the
//! drawing's own millimetres. [`Cutout::build`] then does what
//! [`DepthMap::to_height_field`] and [`Solid::new`] do for a plain depth map,
//! for whatever parameters are current: one conditioned field for the whole
//! drawing — so every piece shares one depth scale — and one solid per piece.
//!
//! # Size
//!
//! A drawing knows how big it is, so by default the print is that size:
//! [`Params::use_drawing_size`]. Turned off, [`Params::size_mm`] sets the
//! outline's longest side instead, as it sets a plaque's.
//!
//! # Where the pieces go
//!
//! Each piece keeps its place in the drawing, centred on the whole outline's
//! bounding box. Exported one file per piece, they open in a slicer already
//! arranged as drawn.

use crate::depth::{DepthMap, HeightField};
use crate::mesh::{Layout, Solid};
use crate::outline::{Outline, Piece};
use crate::params::Params;
use crate::shape::{ShapeError, ShapeMesh};
use crate::{Error, Result};

/// The loaded half: what a drawing gives, before any parameter is applied.
#[derive(Debug, Clone)]
pub struct Cutout {
    /// Height over the outline's bounding box, with coverage where the height
    /// layer does not reach.
    pub map: DepthMap,
    /// The outline in drawing millimetres, y down, origin at the bounding box's
    /// top-left corner.
    pub outline: Outline,
    /// The bounding box's size in drawing millimetres.
    pub size_mm: [f64; 2],
}

/// One piece, ready to be a [`Solid`].
pub struct CutPiece {
    /// The drawing's field, cropped to this piece.
    pub field: HeightField,
    pub layout: Layout,
    pub shape: ShapeMesh,
}

impl CutPiece {
    pub fn solid(&self) -> Solid<'_> {
        Solid::shaped(&self.field, self.layout, &self.shape)
    }
}

/// The built half.
pub struct CutRelief {
    /// The conditioned field over the whole drawing, for the depth preview.
    pub field: HeightField,
    pub pieces: Vec<CutPiece>,
    pub pitch_mm: f32,
    /// Printed footprint of the whole drawing, millimetres.
    pub size_mm: [f32; 2],
}

impl CutRelief {
    /// Width, height and total thickness of the whole drawing as printed.
    pub fn extents_mm(&self, params: &Params) -> [f32; 3] {
        [self.size_mm[0], self.size_mm[1], -params.frame_far()]
    }
}

impl Cutout {
    /// Longest side of the drawing's outline, in the drawing's millimetres.
    pub fn drawing_long_mm(&self) -> f64 {
        self.size_mm[0].max(self.size_mm[1])
    }

    /// Printed size of the outline's bounding box under `params`.
    pub fn printed_size_mm(&self, params: &Params) -> [f64; 2] {
        let scale = self.scale(params);
        [self.size_mm[0] * scale, self.size_mm[1] * scale]
    }

    /// Printed millimetres per drawing millimetre.
    fn scale(&self, params: &Params) -> f64 {
        let long = self.drawing_long_mm().max(1e-9);
        if params.use_drawing_size {
            1.0
        } else {
            params.size_mm as f64 / long
        }
    }

    /// Condition the height data and mesh every piece.
    pub fn build(&self, params: &Params) -> Result<CutRelief> {
        params.validate()?;
        let (gw, gh) = params.grid_for(self.map.width, self.map.height);
        let field = self.map.resized(gw, gh).condition(params);

        // One pitch for the whole drawing, from its longest side. The grid's
        // other side is rounded to whole samples, so the height data may stretch
        // by under a sample; the outline is placed by the pitch and stays exact.
        let scale = self.scale(params);
        let long_printed = self.drawing_long_mm() * scale;
        let pitch = long_printed / (gw.max(gh) - 1) as f64;
        let to_grid = |p: [f64; 2]| [p[0] * scale / pitch, p[1] * scale / pitch];
        let to_view = |g: [f64; 2]| {
            [
                ((g[0] - (gw - 1) as f64 / 2.0) * pitch) as f32,
                (((gh - 1) as f64 / 2.0 - g[1]) * pitch) as f32,
            ]
        };

        let mut pieces: Vec<(Piece, [f64; 4])> = self
            .outline
            .map(to_grid)
            .pieces
            .into_iter()
            .map(|p| {
                let b = p.bounds();
                (p, b)
            })
            .collect();
        // Numbered the way a page reads: top to bottom, then left to right.
        pieces.sort_by(|a, b| {
            (a.1[1].round(), a.1[0])
                .partial_cmp(&(b.1[1].round(), b.1[0]))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut built = Vec::with_capacity(pieces.len());
        for (piece, b) in pieces {
            let clamp_x = |v: f64| (v.max(0.0) as usize).min(gw - 1);
            let clamp_y = |v: f64| (v.max(0.0) as usize).min(gh - 1);
            let (i0, j0) = (clamp_x(b[0].floor()), clamp_y(b[1].floor()));
            let (mut i1, mut j1) = (clamp_x(b[2].ceil()), clamp_y(b[3].ceil()));
            // At least one cell, even for a piece thinner than one.
            if i1 == i0 {
                if i1 + 1 < gw { i1 += 1 } else { continue }
            }
            if j1 == j0 {
                if j1 + 1 < gh { j1 += 1 } else { continue }
            }
            let (w, h) = (i1 - i0 + 1, j1 - j0 + 1);
            let mut z = Vec::with_capacity(w * h);
            for j in j0..=j1 {
                z.extend_from_slice(&field.z[j * gw + i0..=j * gw + i1]);
            }
            let crop = HeightField {
                w,
                h,
                z,
                bits: field.bits,
            };
            let local = Piece {
                loops: piece
                    .loops
                    .iter()
                    .map(|l| {
                        l.iter()
                            .map(|p| [p[0] - i0 as f64, p[1] - j0 as f64])
                            .collect()
                    })
                    .collect(),
            };
            let shape =
                ShapeMesh::build(&crop, &local, params.tolerance_mm).map_err(|e| match e {
                    ShapeError::SelfIntersecting => Error::Outline(e.to_string()),
                    ShapeError::Empty => Error::Outline(e.to_string()),
                })?;
            let (lo, hi) = (to_view([b[0], b[3]]), to_view([b[2], b[1]]));
            let layout = Layout::piece(
                params,
                pitch as f32,
                (gw, gh),
                (i0, j0),
                (w, h),
                [lo[0], lo[1], hi[0], hi[1]],
            );
            built.push(CutPiece {
                field: crop,
                layout,
                shape,
            });
        }
        if built.is_empty() {
            return Err(Error::Outline("the cutout outline has no area".into()));
        }

        Ok(CutRelief {
            field,
            pieces: built,
            pitch_mm: pitch as f32,
            size_mm: [
                (self.size_mm[0] * scale) as f32,
                (self.size_mm[1] * scale) as f32,
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check;
    use crate::depth::BitDepth;
    use crate::outline::{FillRule, Region};

    /// A depth map of `w × h` from a function of normalised position.
    fn map(w: u32, h: u32, f: impl Fn(f32, f32) -> f32) -> DepthMap {
        let mut data = Vec::new();
        for j in 0..h {
            for i in 0..w {
                data.push(f(i as f32 / (w - 1) as f32, j as f32 / (h - 1) as f32));
            }
        }
        DepthMap {
            width: w,
            height: h,
            bits: BitDepth::Sixteen,
            was_colour: false,
            data,
            coverage: None,
        }
    }

    fn circle(cx: f64, cy: f64, r: f64, n: usize) -> Vec<[f64; 2]> {
        (0..n)
            .map(|k| {
                let t = k as f64 / n as f64 * std::f64::consts::TAU;
                [cx + r * t.cos(), cy + r * t.sin()]
            })
            .collect()
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Vec<[f64; 2]> {
        vec![[x, y], [x + w, y], [x + w, y + h], [x, y + h]]
    }

    fn cutout(size: [f64; 2], regions: Vec<Region>, depth: DepthMap) -> Cutout {
        Cutout {
            map: depth,
            outline: Outline::resolve(&regions),
            size_mm: size,
        }
    }

    fn region(loops: Vec<Vec<[f64; 2]>>, rule: FillRule) -> Region {
        Region { loops, rule }
    }

    fn params(px: u32, tolerance: f32) -> Params {
        Params {
            size_px: px,
            tolerance_mm: tolerance,
            emboss_mm: 5.0,
            frame_back_mm: 2.0,
            filter_size: 1,
            ..Params::default()
        }
    }

    fn bumpy() -> DepthMap {
        map(97, 61, |u, v| {
            0.5 + 0.3 * (u * 17.0).sin() * (v * 11.0).cos()
                + 0.2 * ((u - 0.5) * (v - 0.5) * 40.0).sin()
        })
    }

    /// The awkward cases for an outline: a hole, several pieces, concavity, a
    /// sliver thinner than a cell, and edges lying exactly on grid lines.
    fn shapes() -> Vec<(&'static str, [f64; 2], Vec<Region>)> {
        vec![
            (
                "circle",
                [80.0, 50.0],
                vec![region(
                    vec![circle(40.0, 25.0, 24.0, 90)],
                    FillRule::NonZero,
                )],
            ),
            (
                "ring",
                [80.0, 50.0],
                vec![region(
                    vec![circle(40.0, 25.0, 24.0, 90), circle(40.0, 25.0, 10.0, 60)],
                    FillRule::EvenOdd,
                )],
            ),
            (
                "two pieces",
                [80.0, 50.0],
                vec![
                    region(vec![circle(20.0, 25.0, 15.0, 50)], FillRule::NonZero),
                    region(vec![rect(45.0, 5.0, 30.0, 40.0)], FillRule::NonZero),
                ],
            ),
            (
                "concave",
                [80.0, 50.0],
                vec![region(
                    vec![vec![
                        [2.0, 2.0],
                        [78.0, 2.0],
                        [78.0, 48.0],
                        [50.0, 48.0],
                        [50.0, 12.0],
                        [30.0, 12.0],
                        [30.0, 48.0],
                        [2.0, 48.0],
                    ]],
                    FillRule::NonZero,
                )],
            ),
            (
                "sliver",
                [80.0, 50.0],
                vec![region(vec![rect(5.0, 20.0, 70.0, 0.3)], FillRule::NonZero)],
            ),
            (
                "on grid lines",
                [80.0, 50.0],
                vec![region(vec![rect(0.0, 0.0, 80.0, 50.0)], FillRule::NonZero)],
            ),
        ]
    }

    #[test]
    fn every_shape_is_a_closed_solid() {
        for (name, size, regions) in shapes() {
            let c = cutout(size, regions, bumpy());
            for tolerance in [0.0, 0.02, 0.2, 1.0] {
                for px in [64, 97, 160] {
                    let built = c.build(&params(px, tolerance)).unwrap();
                    for (k, piece) in built.pieces.iter().enumerate() {
                        let solid = piece.solid();
                        let report = check::inspect(&solid);
                        assert_eq!(
                            report.watertight(),
                            Some(true),
                            "{name} piece {k} at {px} px, tolerance {tolerance}: {report:?}"
                        );
                        let mut emitted = 0;
                        solid.for_each_triangle(|_| emitted += 1, |_| {});
                        assert_eq!(emitted, solid.triangle_count(), "{name}");
                        for v in 0..solid.vertex_count() {
                            assert!(solid.position(v).is_finite(), "{name} vertex {v}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn pieces_are_counted_and_holes_are_holes() {
        let counts: Vec<(usize, usize)> = shapes()
            .into_iter()
            .map(|(_, size, regions)| {
                let built = cutout(size, regions, bumpy())
                    .build(&params(97, 0.05))
                    .unwrap();
                let holes = built.pieces.iter().map(|p| p.shape.holes()).sum();
                (built.pieces.len(), holes)
            })
            .collect();
        assert_eq!(counts, vec![(1, 0), (1, 1), (2, 0), (1, 0), (1, 0), (1, 0)]);
    }

    /// The discriminating test for an exact edge: a flat disc's volume is
    /// πr²·thickness, and its wall stands at radius r. A staircase edge fails
    /// both.
    #[test]
    fn a_flat_disc_is_a_cylinder() {
        let r = 30.0;
        let c = cutout(
            [80.0, 80.0],
            vec![region(vec![circle(40.0, 40.0, r, 720)], FillRule::NonZero)],
            map(64, 64, |_, _| 0.3),
        );
        let p = params(128, 0.05);
        let built = c.build(&p).unwrap();
        let solid = built.pieces[0].solid();
        let thickness = (-p.frame_far()) as f64;
        let report = check::inspect(&solid);
        assert_eq!(report.watertight(), Some(true), "{report:?}");
        // The 720-gon's own area is a hair under the circle's.
        let polygon = 0.5 * 720.0 * r * r * (std::f64::consts::TAU / 720.0).sin();
        let want = polygon * thickness;
        assert!(
            (report.volume_mm3 - want).abs() / want < 1e-3,
            "volume {} vs {want}",
            report.volume_mm3
        );
        let shape = solid.shape().unwrap();
        for k in 0..shape.ring_len() {
            let p = solid.position(solid.vertex_count() - shape.ring_len() + k);
            let radius = (p.x as f64).hypot(p.y as f64);
            assert!(
                radius <= r + 1e-3 && radius > r * 0.999,
                "wall at radius {radius}"
            );
        }
    }

    #[test]
    fn the_drawing_sets_the_size_unless_overridden() {
        let c = cutout(
            [80.0, 50.0],
            vec![region(vec![rect(0.0, 0.0, 80.0, 50.0)], FillRule::NonZero)],
            bumpy(),
        );
        let own = c.build(&params(97, 0.05)).unwrap();
        assert!((own.size_mm[0] - 80.0).abs() < 1e-4 && (own.size_mm[1] - 50.0).abs() < 1e-4);

        let p = Params {
            use_drawing_size: false,
            size_mm: 160.0,
            ..params(97, 0.05)
        };
        let scaled = c.build(&p).unwrap();
        assert!((scaled.size_mm[0] - 160.0).abs() < 1e-3);
        let e = scaled.pieces[0].layout.extents_mm();
        assert!(
            (e.x - 160.0).abs() < 1e-2 && (e.y - 100.0).abs() < 1e-2,
            "{e}"
        );
    }

    /// An asymmetric shape, to catch a mirrored model: the notch drawn at the
    /// drawing's top-left must print at the top-left as seen from the front.
    #[test]
    fn nothing_is_mirrored() {
        let c = cutout(
            [80.0, 50.0],
            vec![
                region(vec![rect(0.0, 0.0, 10.0, 10.0)], FillRule::NonZero),
                region(vec![rect(30.0, 20.0, 50.0, 30.0)], FillRule::NonZero),
            ],
            bumpy(),
        );
        let built = c.build(&params(97, 0.05)).unwrap();
        // Read order: the top-left square comes first.
        let small = &built.pieces[0];
        let b = small.layout.bounds_mm;
        assert!(b[0] < -30.0 && b[3] > 20.0, "small square at {b:?}");
        let big = &built.pieces[1].layout.bounds_mm;
        assert!(big[2] > 30.0 && big[1] < -20.0, "big rectangle at {big:?}");
        // And the solid really is there, not just its bounds.
        let s = small.solid();
        let centre = (0..s.vertex_count())
            .map(|v| s.position(v))
            .fold(glam::Vec3::ZERO, |a, p| a + p)
            / s.vertex_count() as f32;
        assert!(centre.x < -30.0 && centre.y > 15.0, "{centre}");
    }

    #[test]
    fn uncovered_parts_of_the_outline_rest_on_the_back_plate() {
        // The height data covers only the left half.
        let mut depth = map(64, 40, |_, _| 0.0);
        depth.coverage = Some(
            (0..64 * 40)
                .map(|k| if k % 64 < 32 { 1.0 } else { 0.0 })
                .collect(),
        );
        let c = cutout(
            [80.0, 50.0],
            vec![region(vec![rect(0.0, 0.0, 80.0, 50.0)], FillRule::NonZero)],
            depth,
        );
        let p = params(64, 0.0);
        let built = c.build(&p).unwrap();
        let s = built.pieces[0].solid();
        let report = check::inspect(&s);
        assert_eq!(report.watertight(), Some(true));
        for v in 0..s.vertex_count() {
            let q = s.position(v);
            if q.x > 5.0 && q.z > p.frame_far() + 0.01 {
                assert!((q.z + p.emboss_mm).abs() < 1e-3, "{q} should be at -emboss");
            }
        }
    }
}
