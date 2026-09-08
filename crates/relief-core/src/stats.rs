//! The numbers the status bar shows.
//!
//! Two of them are the point of this module. **Step size** is relief depth
//! divided by the number of levels the source could express; when it exceeds the
//! layer height, the depth map's quantisation is coarser than the printer and
//! shallow gradients terrace. That — not the file format — is the honest test of
//! whether an 8-bit JPEG is good enough, and it usually is: 20 mm of relief from
//! an 8-bit map steps every 0.078 mm, comfortably under a 0.1 mm layer.
//!
//! **Triangle count** is the other. Lifting a resolution cap is easy; the file it
//! produces still has to open in a slicer, and past a few million triangles that
//! stops being true. The app warns on this number rather than refusing a
//! resolution.
//!
//! Everything here is millimetres. [`Params::units`] is about the *file*, not the
//! readouts — it appears in [`Stats::export_units`] only so a caller can say
//! which unit the numbers in the file will be in.

use crate::check;
use crate::depth::BitDepth;
use crate::export::Format;
use crate::mesh::Solid;
use crate::params::{Params, TRIANGLE_WARN, Units};

/// PLA, near enough for an estimate a person uses to decide whether to print.
const PLA_G_PER_MM3: f32 = 1.24e-3;

#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub grid: (usize, usize),
    pub vertices: usize,
    pub triangles: usize,
    /// Bounding box in millimetres: width, height, total thickness.
    pub bbox_mm: [f32; 3],
    /// How far the relief stands out of its plate.
    pub relief_mm: f32,
    /// Vertex spacing on the printed model — the finest detail it can carry.
    pub pitch_mm: f32,
    /// Whether the solid has a bezel at all.
    pub framed: bool,
    /// Triangles the same relief would need without simplification.
    pub full_triangles: usize,
    /// Simplification tolerance asked for, and the deviation actually reached.
    pub tolerance_mm: f32,
    pub achieved_error_mm: f32,
    /// Levels the source could express: 256 for 8-bit, 65536 for 16-bit.
    pub levels: u32,
    pub bits: BitDepth,
    /// Millimetres of relief per source level.
    pub step_mm: f32,
    pub layer_mm: f32,
    pub volume_mm3: f64,
    pub filament_g: f32,
    pub file_bytes: u64,
    pub format: Format,
    /// The unit the exported file's numbers will be written in.
    pub export_units: Units,
}

impl Stats {
    pub fn new(solid: &Solid, format: Format, params: &Params) -> Self {
        let layout = &solid.layout;
        let report = check::summary(solid);
        let bbox = layout.extents_mm();
        let relief_mm = layout.relief_mm();
        let levels = solid.field().bits.levels();

        Self {
            grid: (layout.w, layout.h),
            vertices: report.vertices,
            triangles: report.triangles,
            bbox_mm: [bbox.x, bbox.y, bbox.z],
            relief_mm,
            pitch_mm: layout.pitch_mm,
            framed: layout.framed(),
            full_triangles: layout.triangle_count(),
            tolerance_mm: params.tolerance_mm,
            achieved_error_mm: solid.decimation().map_or(0.0, |d| d.max_error_mm()),
            levels,
            bits: solid.field().bits,
            step_mm: relief_mm / (levels - 1) as f32,
            layer_mm: params.layer_mm,
            volume_mm3: report.volume_mm3,
            filament_g: report.volume_mm3 as f32 * PLA_G_PER_MM3,
            file_bytes: format.estimated_bytes(solid),
            format,
            export_units: params.units,
        }
    }

    /// Is the depth map's quantisation finer than the printer's layers?
    pub fn steps_ok(&self) -> bool {
        self.step_mm <= self.layer_mm
    }

    /// Is the surface simplified, and did it achieve anything?
    pub fn simplified(&self) -> bool {
        self.tolerance_mm > 0.0 && self.triangles < self.full_triangles
    }

    /// Fraction of the full-resolution triangles removed.
    pub fn reduction(&self) -> f32 {
        if self.full_triangles == 0 {
            return 0.0;
        }
        1.0 - self.triangles as f32 / self.full_triangles as f32
    }

    /// Is the tolerance finer than the source can express?
    ///
    /// Below the quantisation step there is nothing to simplify: every step is a
    /// feature. This is the difference between a simplifier that looks broken
    /// and one that tells you why.
    pub fn tolerance_below_source(&self) -> bool {
        self.tolerance_mm > 0.0 && self.tolerance_mm < self.step_mm
    }

    /// What simplification did, phrased for a person.
    pub fn simplify_line(&self) -> String {
        if self.tolerance_mm <= 0.0 {
            return "off — every grid vertex is kept".into();
        }
        if self.tolerance_below_source() {
            return format!(
                "blocked: {:.3} mm is finer than the source's {:.3} mm step, so                  every step has to be kept. Raise the tolerance or pre-smooth.",
                self.tolerance_mm, self.step_mm
            );
        }
        format!(
            "−{:.0}% ({} from {}), worst deviation {:.3} mm of {:.3} allowed",
            100.0 * self.reduction(),
            self.triangles,
            self.full_triangles,
            self.achieved_error_mm,
            self.tolerance_mm
        )
    }

    /// Will this many triangles give a slicer trouble?
    pub fn slicer_strain(&self) -> bool {
        self.triangles > TRIANGLE_WARN
    }

    pub fn bbox_line(&self) -> String {
        let [x, y, z] = self.bbox_mm;
        format!("{x:.1} × {y:.1} × {z:.1} mm")
    }

    pub fn relief_line(&self) -> String {
        format!("{:.1} mm", self.relief_mm)
    }

    /// Vertex spacing, phrased as the detail the print can carry.
    pub fn pitch_line(&self) -> String {
        format!("{:.3} mm/vertex", self.pitch_mm)
    }

    /// The step-size verdict, phrased for a person.
    pub fn step_line(&self) -> String {
        let step = self.step_mm;
        let precision = if step < 0.01 { 4 } else { 3 };
        if self.steps_ok() {
            format!(
                "{step:.precision$} mm/level — finer than a {:.2} mm layer",
                self.layer_mm
            )
        } else {
            format!(
                "{step:.precision$} mm/level — coarser than a {:.2} mm layer; \
                 raise Smooth radius, lower Emboss height, or use a 16-bit map",
                self.layer_mm
            )
        }
    }

    pub fn file_line(&self) -> String {
        format!(
            "{} {} in {}",
            human_bytes(self.file_bytes),
            self.format.label(),
            self.export_units.long_label()
        )
    }
}

/// Bytes as a person reads them.
pub fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} kB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::{BitDepth, HeightField};

    fn field(w: usize, h: usize, bits: BitDepth, depth_mm: f32) -> HeightField {
        HeightField {
            w,
            h,
            z: (0..w * h)
                .map(|k| -depth_mm * (k % w) as f32 / (w - 1) as f32)
                .collect(),
            bits,
        }
    }

    /// The bit-depth table from the plan, recomputed here so the advice the UI
    /// gives cannot drift from the arithmetic behind it.
    #[test]
    fn step_size_matches_the_published_table() {
        let cases = [
            // (relief mm, bits, expected mm/level, ok at a 0.1 mm layer?)
            (20.0, BitDepth::Eight, 0.0784, true),
            (20.0, BitDepth::Sixteen, 0.0003, true),
            (40.0, BitDepth::Eight, 0.1569, false),
            (120.0, BitDepth::Eight, 0.4706, false),
        ];
        for (relief, bits, want, ok) in cases {
            let params = Params {
                emboss_mm: relief,
                layer_mm: 0.1,
                ..Params::default()
            };
            let f = field(65, 49, bits, relief);
            let solid = Solid::new(&f, &params);
            let stats = Stats::new(&solid, Format::Stl, &params);
            assert!(
                (stats.step_mm - want).abs() < 0.001,
                "{relief} mm {bits:?}: {} vs {want}",
                stats.step_mm
            );
            assert_eq!(stats.steps_ok(), ok, "verdict for {relief} mm {bits:?}");
        }
    }

    #[test]
    fn stl_estimate_is_fifty_bytes_a_triangle() {
        let params = Params::default();
        let f = field(33, 25, BitDepth::Sixteen, 20.0);
        let solid = Solid::new(&f, &params);
        let stats = Stats::new(&solid, Format::Stl, &params);
        assert_eq!(stats.file_bytes, 84 + 50 * stats.triangles as u64);
    }

    /// Readouts are metric whatever the export unit is; only the file changes.
    #[test]
    fn readouts_are_millimetres_whatever_the_export_unit() {
        let f = field(9, 9, BitDepth::Sixteen, 20.0);
        let metric = Params {
            size_mm: 254.0,
            ..Params::default()
        };
        let imperial = Params {
            units: Units::In,
            ..metric
        };
        let a = Stats::new(&Solid::new(&f, &metric), Format::Stl, &metric);
        let b = Stats::new(&Solid::new(&f, &imperial), Format::Stl, &imperial);
        assert_eq!(a.bbox_line(), b.bbox_line());
        assert!(a.bbox_line().starts_with("254.0"), "{}", a.bbox_line());
        assert!(a.bbox_line().ends_with(" mm"));
        // The file line is where the unit shows up.
        assert!(a.file_line().ends_with("in millimetres"));
        assert!(b.file_line().ends_with("in inches"));
    }

    #[test]
    fn pitch_is_the_detail_the_print_can_carry() {
        let params = Params {
            size_mm: 100.0,
            frame_thickness_mm: 5.0,
            ..Params::default()
        };
        let f = field(91, 91, BitDepth::Sixteen, 20.0);
        let stats = Stats::new(&Solid::new(&f, &params), Format::Stl, &params);
        // 90 mm of relief across 90 gaps.
        assert!((stats.pitch_mm - 1.0).abs() < 1e-4, "{}", stats.pitch_mm);
        assert_eq!(stats.pitch_line(), "1.000 mm/vertex");
    }

    #[test]
    fn bytes_read_the_way_people_write_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(20_500_000), "19.6 MB");
        assert_eq!(human_bytes(1_350_000_000), "1.26 GB");
    }
}
