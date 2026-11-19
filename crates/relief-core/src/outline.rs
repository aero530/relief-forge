//! The cutout: closed outlines resolved into separate pieces with holes.
//!
//! A drawing describes a cutout as any number of paths, each with its own fill
//! rule, overlapping however they like. [`Outline::resolve`] turns that into
//! what the mesh needs: a list of **pieces**, each one outer loop plus the holes
//! inside it, none of them touching another piece. Each piece becomes its own
//! solid — "separate pieces are separate models".
//!
//! Every path is resolved under its own fill rule first, then all of them are
//! unioned, which is what a drawing program shows: two overlapping shapes on
//! one layer read as one filled area, and a hole in one is filled again by
//! another drawn over it.
//!
//! Coordinates here are whatever the caller hands in; the SVG reader gives
//! drawing millimetres with y pointing down, and [`crate::shape`] converts to
//! grid units. Nothing in this module cares which way y points except
//! [`Piece::oriented`], which says so.

use i_overlay::core::fill_rule::FillRule as OverlayFill;
use i_overlay::float::simplify::SimplifyShape;

/// Which side of a set of loops is inside, as SVG's `fill-rule` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

impl From<FillRule> for OverlayFill {
    fn from(rule: FillRule) -> Self {
        match rule {
            FillRule::NonZero => OverlayFill::NonZero,
            FillRule::EvenOdd => OverlayFill::EvenOdd,
        }
    }
}

/// A filled area as drawn: closed loops, and the rule that decides what is inside.
#[derive(Debug, Clone, Default)]
pub struct Region {
    pub loops: Vec<Vec<[f64; 2]>>,
    pub rule: FillRule,
}

/// One connected piece of the cutout: its outer loop first, then its holes.
///
/// Loops are open polylines — the closing edge back to the first point is
/// implied, never repeated.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub loops: Vec<Vec<[f64; 2]>>,
}

/// The whole cutout, resolved.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outline {
    pub pieces: Vec<Piece>,
}

/// Pieces and holes smaller than this many square units are dropped as noise —
/// the slivers a boolean leaves where two edges almost coincide.
const MIN_AREA: f64 = 1e-6;

impl Outline {
    /// Resolve drawn regions into separate pieces.
    pub fn resolve(regions: &[Region]) -> Self {
        // Each region under its own rule. The results come back with every
        // outer loop wound one way and every hole the other, so feeding them
        // all into one non-zero pass is exactly their union.
        let mut contours: Vec<Vec<[f64; 2]>> = Vec::new();
        for region in regions {
            let loops: Vec<Vec<[f64; 2]>> = region
                .loops
                .iter()
                .map(|l| clean(l))
                .filter(|l| l.len() >= 3)
                .collect();
            if loops.is_empty() {
                continue;
            }
            for shape in loops.simplify_shape_as::<i64>(region.rule.into()) {
                contours.extend(shape);
            }
        }
        if contours.is_empty() {
            return Self::default();
        }

        let pieces = contours
            .simplify_shape_as::<i64>(OverlayFill::NonZero)
            .into_iter()
            .filter_map(|shape| {
                let mut loops = shape.into_iter().map(|l| clean(&l));
                let outer = loops.next()?;
                if outer.len() < 3 || signed_area(&outer).abs() < MIN_AREA {
                    return None;
                }
                let mut all = vec![outer];
                all.extend(loops.filter(|l| l.len() >= 3 && signed_area(l).abs() >= MIN_AREA));
                Some(Piece { loops: all })
            })
            .collect();
        Self { pieces }
    }

    /// `[min_x, min_y, max_x, max_y]` over every piece, or `None` when empty.
    pub fn bounds(&self) -> Option<[f64; 4]> {
        self.pieces.iter().map(Piece::bounds).reduce(|a, b| {
            [
                a[0].min(b[0]),
                a[1].min(b[1]),
                a[2].max(b[2]),
                a[3].max(b[3]),
            ]
        })
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// Apply `f` to every point.
    pub fn map(&self, f: impl Fn([f64; 2]) -> [f64; 2]) -> Self {
        Self {
            pieces: self
                .pieces
                .iter()
                .map(|p| Piece {
                    loops: p
                        .loops
                        .iter()
                        .map(|l| l.iter().map(|&q| f(q)).collect())
                        .collect(),
                })
                .collect(),
        }
    }
}

impl Piece {
    /// `[min_x, min_y, max_x, max_y]`.
    pub fn bounds(&self) -> [f64; 4] {
        let mut b = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for p in self.loops.iter().flatten() {
            b[0] = b[0].min(p[0]);
            b[1] = b[1].min(p[1]);
            b[2] = b[2].max(p[0]);
            b[3] = b[3].max(p[1]);
        }
        b
    }

    /// Filled area: the outer loop less its holes.
    pub fn area(&self) -> f64 {
        let mut loops = self.loops.iter();
        let outer = loops.next().map_or(0.0, |l| signed_area(l).abs());
        outer - loops.map(|l| signed_area(l).abs()).sum::<f64>()
    }

    /// Number of holes — each one a tunnel through the finished solid.
    pub fn holes(&self) -> usize {
        self.loops.len().saturating_sub(1)
    }

    /// The same loops, wound for a solid: the outer loop counter-clockwise and
    /// every hole clockwise **in a y-down space** seen with y flipped — that is,
    /// counter-clockwise and clockwise as a person looks at the front of the
    /// relief. Walking any loop, the material is then on the left.
    pub fn oriented(&self) -> Self {
        let loops = self
            .loops
            .iter()
            .enumerate()
            .map(|(k, l)| {
                // Shoelace in y-down space is negated relative to y-up, so a
                // loop that reads counter-clockwise from the front has a
                // *negative* area here.
                let ccw_from_front = signed_area(l) < 0.0;
                let want_ccw = k == 0;
                if ccw_from_front == want_ccw {
                    l.clone()
                } else {
                    l.iter().rev().copied().collect()
                }
            })
            .collect();
        Self { loops }
    }

    /// Split every edge longer than `max_len` into equal parts no longer than it.
    pub fn resampled(&self, max_len: f64) -> Self {
        let loops = self
            .loops
            .iter()
            .map(|l| {
                let mut out = Vec::with_capacity(l.len());
                for (k, &a) in l.iter().enumerate() {
                    let b = l[(k + 1) % l.len()];
                    out.push(a);
                    let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
                    let parts = (len / max_len).ceil() as usize;
                    for s in 1..parts {
                        let t = s as f64 / parts as f64;
                        out.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
                    }
                }
                out
            })
            .collect();
        Self { loops }
    }
}

/// Shoelace area: positive for counter-clockwise in a y-up space.
pub fn signed_area(l: &[[f64; 2]]) -> f64 {
    let mut a = 0.0;
    for (k, p) in l.iter().enumerate() {
        let q = l[(k + 1) % l.len()];
        a += p[0] * q[1] - q[0] * p[1];
    }
    a / 2.0
}

/// Drop repeated points, including a closing point that repeats the first.
fn clean(l: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let same = |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1e-9 && (a[1] - b[1]).abs() < 1e-9;
    let mut out: Vec<[f64; 2]> = Vec::with_capacity(l.len());
    for &p in l {
        if out.last().is_none_or(|&q| !same(p, q)) {
            out.push(p);
        }
    }
    while out.len() > 1 && same(out[0], out[out.len() - 1]) {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, s: f64) -> Vec<[f64; 2]> {
        vec![[x, y], [x + s, y], [x + s, y + s], [x, y + s]]
    }

    #[test]
    fn separate_shapes_are_separate_pieces() {
        let outline = Outline::resolve(&[
            Region {
                loops: vec![square(0.0, 0.0, 10.0)],
                rule: FillRule::NonZero,
            },
            Region {
                loops: vec![square(20.0, 0.0, 10.0)],
                rule: FillRule::NonZero,
            },
        ]);
        assert_eq!(outline.pieces.len(), 2);
        assert_eq!(outline.bounds(), Some([0.0, 0.0, 30.0, 10.0]));
    }

    #[test]
    fn overlapping_shapes_merge() {
        let outline = Outline::resolve(&[
            Region {
                loops: vec![square(0.0, 0.0, 10.0)],
                rule: FillRule::NonZero,
            },
            Region {
                loops: vec![square(5.0, 5.0, 10.0)],
                rule: FillRule::NonZero,
            },
        ]);
        assert_eq!(outline.pieces.len(), 1);
        assert!((outline.pieces[0].area() - 175.0).abs() < 1e-6);
    }

    #[test]
    fn the_fill_rule_decides_whether_a_hole_is_a_hole() {
        // Both loops wound the same way: even-odd cuts a hole, non-zero fills it.
        let loops = vec![square(0.0, 0.0, 10.0), square(3.0, 3.0, 4.0)];
        let even_odd = Outline::resolve(&[Region {
            loops: loops.clone(),
            rule: FillRule::EvenOdd,
        }]);
        assert_eq!(even_odd.pieces.len(), 1);
        assert_eq!(even_odd.pieces[0].holes(), 1);
        assert!((even_odd.pieces[0].area() - 84.0).abs() < 1e-6);

        let non_zero = Outline::resolve(&[Region {
            loops,
            rule: FillRule::NonZero,
        }]);
        assert_eq!(non_zero.pieces[0].holes(), 0);
        assert!((non_zero.pieces[0].area() - 100.0).abs() < 1e-6);
    }

    #[test]
    fn a_shape_drawn_over_a_hole_fills_it() {
        let outline = Outline::resolve(&[
            Region {
                loops: vec![square(0.0, 0.0, 10.0), square(3.0, 3.0, 4.0)],
                rule: FillRule::EvenOdd,
            },
            Region {
                loops: vec![square(2.0, 2.0, 6.0)],
                rule: FillRule::NonZero,
            },
        ]);
        assert_eq!(outline.pieces.len(), 1);
        assert_eq!(outline.pieces[0].holes(), 0);
    }

    #[test]
    fn orientation_puts_the_material_on_the_left() {
        let outline = Outline::resolve(&[Region {
            loops: vec![square(0.0, 0.0, 10.0), square(3.0, 3.0, 4.0)],
            rule: FillRule::EvenOdd,
        }]);
        let piece = outline.pieces[0].oriented();
        // y-down space: counter-clockwise from the front is negative here.
        assert!(signed_area(&piece.loops[0]) < 0.0);
        assert!(signed_area(&piece.loops[1]) > 0.0);
    }

    #[test]
    fn resampling_bounds_every_edge() {
        let piece = Piece {
            loops: vec![square(0.0, 0.0, 10.0)],
        }
        .resampled(3.0);
        let l = &piece.loops[0];
        assert_eq!(l.len(), 16);
        for (k, a) in l.iter().enumerate() {
            let b = l[(k + 1) % l.len()];
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            assert!(len <= 3.0 + 1e-9);
        }
    }
}
