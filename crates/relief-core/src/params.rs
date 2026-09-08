//! The controls, their ranges, and their defaults.
//!
//! # Everything physical is millimetres
//!
//! Upstream expresses relief depth and frame sizes as fractions of the model's
//! longest side, so "embossing 20" means nothing until you also know how big the
//! plaque is. Here they are millimetres, because that is what a person setting
//! up a print is actually thinking in, and because it makes the controls
//! independent: changing the relief depth no longer changes the plaque's width.
//!
//! The near and far planes stay fractions — they window the *depth map's* own
//! range, which has no physical unit until the emboss height gives it one.
//!
//! Upstream's parameters convert directly: a fraction times the old model scale.
//! [`Params::example`] carries its published example over that way.

use serde::{Deserialize, Serialize};

/// Longest side of the mesh grid, in pixels.
///
/// Upstream stops at 1024. We go to 4096 because the export path streams and the
/// preview is chunked, but a warning arrives long before that: past a few million
/// triangles the file, not the app, becomes the problem.
pub const SIZE_PX_MIN: u32 = 256;
pub const SIZE_PX_MAX: u32 = 4096;
/// Upstream's ceiling, and the point past which the app starts warning.
pub const SIZE_PX_UPSTREAM_MAX: u32 = 1024;
/// Triangle count past which a slicer starts to struggle.
pub const TRIANGLE_WARN: usize = 3_000_000;

/// Printed size limits, in millimetres.
pub const SIZE_MM_MIN: f32 = 10.0;
pub const SIZE_MM_MAX: f32 = 1000.0;
/// Relief depth limits, in millimetres.
pub const EMBOSS_MM_MIN: f32 = 0.2;
pub const EMBOSS_MM_MAX: f32 = 100.0;
/// Simplification tolerance limits, in millimetres. Zero switches it off.
///
/// A millimetre is well past anything a print needs — at that point the
/// simplification is visible in the relief itself — but a deliberately coarse
/// mesh is a legitimate thing to want, so the ceiling allows it.
pub const TOLERANCE_MM_MAX: f32 = 1.0;

/// Which way up the exported mesh sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Orientation {
    /// Relief facing +Z, back plate on the bed. The natural way to print a
    /// plaque, and identical to this crate's working space, so it costs no
    /// transform.
    #[default]
    FaceUp,
    /// Rotated −90° about X, standing the plaque up with the image's vertical
    /// axis along Z. What upstream always does, and the right choice for a
    /// lithophane: the relief gradient then runs across layers instead of along
    /// them, so shallow slopes don't terrace.
    Upright,
}

impl Orientation {
    pub const ALL: [Orientation; 2] = [Orientation::FaceUp, Orientation::Upright];

    pub fn label(self) -> &'static str {
        match self {
            Orientation::FaceUp => "Face up",
            Orientation::Upright => "Upright",
        }
    }
}

/// The unit the exported file's numbers are written in.
///
/// STL, OBJ and PLY all store bare numbers with no unit field, so the unit is a
/// convention between the writer and the reader. Slicers assume millimetres,
/// which is why that is the default; some older CAD and CAM workflows assume
/// inches, and for those the coordinates themselves have to be divided.
///
/// This does not change the model — only the numbers written into the file, and
/// only for the file. Every readout in the app stays metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Units {
    #[default]
    Mm,
    In,
}

impl Units {
    pub const ALL: [Units; 2] = [Units::Mm, Units::In];

    pub fn label(self) -> &'static str {
        match self {
            Units::Mm => "mm",
            Units::In => "in",
        }
    }

    pub fn long_label(self) -> &'static str {
        match self {
            Units::Mm => "millimetres",
            Units::In => "inches",
        }
    }

    /// How many of this unit make a millimetre — the factor exported
    /// coordinates are multiplied by.
    pub fn per_mm(self) -> f32 {
        match self {
            Units::Mm => 1.0,
            Units::In => 1.0 / 25.4,
        }
    }

    /// Convert a millimetre value into this unit.
    pub fn from_mm(self, mm: f32) -> f32 {
        mm * self.per_mm()
    }
}

/// Every knob the geometry depends on.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Params {
    /// `coef_near`: depths in front of this clamp flat. A fraction of the depth
    /// map's own range, 0…1.
    pub near: f32,
    /// `coef_far`: depths behind this clamp to a plateau. 0…1.
    pub far: f32,
    /// `emboss`, in millimetres: how far the relief stands out of its plate.
    pub emboss_mm: f32,
    /// Flip the depth map. Marigold writes near as dark; plenty of other tools
    /// do the opposite, and without this those maps emboss inside-out.
    pub invert: bool,
    /// `size_longest_px`: mesh resolution — the depth map is resampled so its
    /// longest side is this many pixels, and each pixel becomes one vertex.
    pub size_px: u32,
    /// Longest outer side of the finished plaque, frame included, in millimetres.
    pub size_mm: f32,
    /// `filter_size`: median window in pixels, odd, 1…5. 1 disables it.
    pub filter_size: u32,
    /// Gaussian blur in pixels, applied to 8-bit inputs only, to break the
    /// 256-level staircase before it becomes geometry. 0 disables it.
    pub presmooth: f32,
    /// `f_thic`: bezel width outside the relief, in millimetres.
    /// **Zero means no frame at all** — see [`Params::framed`].
    pub frame_thickness_mm: f32,
    /// `f_near`: height of the bezel face above the front of the relief.
    /// Negative sinks it in; it can never pass the deepest point. Meaningless
    /// without a frame.
    pub frame_near_mm: f32,
    /// `f_back`: back plate thickness behind the deepest point.
    pub frame_back_mm: f32,
    /// How far the simplified surface may deviate from the height field, in
    /// millimetres. Zero keeps every grid vertex.
    ///
    /// Asking for less than the depth map's own quantisation step gains nothing
    /// — every step becomes a feature that has to be preserved. See
    /// [`crate::decimate`].
    pub tolerance_mm: f32,
    /// Printer layer height, for the step-size check. Not geometry.
    pub layer_mm: f32,
    pub orientation: Orientation,
    /// The unit the exported file is written in. Readouts stay metric.
    pub units: Units,
}

impl Default for Params {
    /// Upstream's defaults, converted: its fractions against the 100 mm plaque
    /// they were designed around.
    fn default() -> Self {
        Self {
            near: 0.0,
            far: 1.0,
            emboss_mm: 20.0,
            invert: false,
            size_px: 512,
            size_mm: 100.0,
            filter_size: 3,
            presmooth: 0.0,
            frame_thickness_mm: 5.0,
            frame_near_mm: 1.0,
            frame_back_mm: 1.0,
            // The one default that is not upstream's, because upstream has no
            // equivalent: half the default layer height, which is below what the
            // printer can resolve and typically removes most of the triangles.
            tolerance_mm: 0.05,
            layer_mm: 0.1,
            orientation: Orientation::FaceUp,
            units: Units::Mm,
        }
    }
}

impl Params {
    /// The values the upstream demo ships as its Einstein example, converted to
    /// millimetres against its 10 cm plaque.
    pub fn example() -> Self {
        Self {
            near: 0.0,
            far: 0.5,
            emboss_mm: 45.0,
            size_px: 512,
            size_mm: 100.0,
            filter_size: 3,
            frame_thickness_mm: 5.0,
            frame_near_mm: -22.0,
            frame_back_mm: 1.0,
            ..Self::default()
        }
    }

    /// Is there a frame at all?
    ///
    /// A zero-width bezel is not a thin frame, it is no frame: the relief's
    /// sides drop straight to the back plate, and the rim, bezel and near offset
    /// stop existing rather than becoming degenerate slivers a slicer has to
    /// think about.
    pub fn framed(&self) -> bool {
        self.frame_thickness_mm > 0.0
    }

    /// Clamp every field into its legal range, in place.
    ///
    /// Called before the geometry reads them, so a typed-in value can never
    /// produce a mesh that makes no sense. The one thing this cannot fix is an
    /// empty depth window, which is a real error — see [`Params::validate`].
    pub fn clamp(&mut self) {
        self.near = self.near.clamp(0.0, 1.0);
        self.far = self.far.clamp(0.0, 1.0);
        self.size_px = self.size_px.clamp(SIZE_PX_MIN, SIZE_PX_MAX);
        self.size_mm = self.size_mm.clamp(SIZE_MM_MIN, SIZE_MM_MAX);
        self.emboss_mm = self.emboss_mm.clamp(EMBOSS_MM_MIN, EMBOSS_MM_MAX);
        // Odd only: a median window has to have a middle.
        self.filter_size = self.filter_size.clamp(1, 5) | 1;
        self.presmooth = self.presmooth.clamp(0.0, 2.0);
        // The frame cannot eat the picture: leave at least 2 mm of relief across.
        let widest_frame = ((self.size_mm - 2.0) / 2.0).max(0.0);
        self.frame_thickness_mm = self.frame_thickness_mm.clamp(0.0, widest_frame);
        self.frame_near_mm = self.frame_near_mm.clamp(-EMBOSS_MM_MAX, EMBOSS_MM_MAX);
        self.frame_back_mm = self.frame_back_mm.clamp(0.2, 25.0);
        self.tolerance_mm = self.tolerance_mm.clamp(0.0, TOLERANCE_MM_MAX);
        self.layer_mm = self.layer_mm.clamp(0.02, 0.4);
    }

    pub fn validate(&self) -> crate::Result<()> {
        if self.far <= self.near {
            return Err(crate::Error::EmptyDepthWindow {
                near: self.near,
                far: self.far,
            });
        }
        Ok(())
    }

    /// Bezel plane in millimetres, clamped so the frame face can never sit
    /// behind the deepest point of the relief, and zero when there is no frame.
    pub fn frame_near_clamped(&self) -> f32 {
        if self.framed() {
            self.frame_near_mm.max(-self.emboss_mm)
        } else {
            0.0
        }
    }

    /// Back of the solid, in millimetres (negative).
    pub fn frame_far(&self) -> f32 {
        -self.emboss_mm - self.frame_back_mm
    }

    /// Is the surface simplified at all?
    pub fn simplifies(&self) -> bool {
        self.tolerance_mm > 0.0
    }

    /// Grid dimensions for a source image of this aspect, longest side
    /// [`Params::size_px`].
    ///
    /// Always at least 2 in each axis: one quad is the smallest thing that can
    /// be a surface.
    pub fn grid_for(&self, src_w: u32, src_h: u32) -> (usize, usize) {
        let longest = src_w.max(src_h).max(1);
        let w = (self.size_px as u64 * src_w as u64 / longest as u64).max(2) as usize;
        let h = (self.size_px as u64 * src_h as u64 / longest as u64).max(2) as usize;
        (w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_upstreams_in_millimetres() {
        let p = Params::default();
        assert_eq!(p.near, 0.0);
        assert_eq!(p.far, 1.0);
        assert_eq!(p.size_px, 512);
        assert_eq!(p.filter_size, 3);
        // Upstream's 20% emboss and 5% bezel, on the 100 mm plaque they assume.
        assert_eq!(p.size_mm, 100.0);
        assert_eq!(p.emboss_mm, 20.0);
        assert_eq!(p.frame_thickness_mm, 5.0);
    }

    #[test]
    fn clamp_forces_odd_filter() {
        let mut p = Params {
            filter_size: 4,
            ..Params::default()
        };
        p.clamp();
        assert_eq!(p.filter_size, 5);

        let mut p = Params {
            filter_size: 99,
            ..Params::default()
        };
        p.clamp();
        assert_eq!(p.filter_size, 5);
    }

    #[test]
    fn a_frame_cannot_eat_the_picture() {
        let mut p = Params {
            size_mm: 50.0,
            frame_thickness_mm: 40.0,
            ..Params::default()
        };
        p.clamp();
        assert_eq!(p.frame_thickness_mm, 24.0);
        assert!(p.framed());
    }

    #[test]
    fn zero_thickness_means_no_frame() {
        let p = Params {
            frame_thickness_mm: 0.0,
            frame_near_mm: 8.0,
            ..Params::default()
        };
        assert!(!p.framed());
        // With no frame there is no bezel face to offset.
        assert_eq!(p.frame_near_clamped(), 0.0);
    }

    #[test]
    fn empty_window_is_an_error() {
        let p = Params {
            near: 0.6,
            far: 0.5,
            ..Params::default()
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn grid_keeps_aspect_and_never_degenerates() {
        let p = Params {
            size_px: 512,
            ..Params::default()
        };
        assert_eq!(p.grid_for(1536, 1152), (512, 384));
        assert_eq!(p.grid_for(1152, 1536), (384, 512));
        // A pathological aspect still yields a meshable grid.
        assert_eq!(p.grid_for(10_000, 1), (512, 2));
    }

    #[test]
    fn inches_only_scale_the_export() {
        assert_eq!(Units::Mm.per_mm(), 1.0);
        assert!((Units::In.from_mm(25.4) - 1.0).abs() < 1e-6);
    }
}
