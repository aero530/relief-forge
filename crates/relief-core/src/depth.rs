//! Decoding a depth map, and conditioning it into a height field.
//!
//! Upstream refuses anything that is not a 16-bit PNG. We accept 8-bit input —
//! JPEG included — because the limit that actually matters is millimetres of
//! relief per level against the printer's layer height, which is a computed
//! number rather than a property of the file format (see [`crate::stats`]).
//! What 8-bit input does cost is honesty about two things, both handled here:
//! the 256-level staircase, which [`Params::presmooth`] breaks up, and JPEG's
//! 8×8 block ringing, which the median filter removes.

use crate::params::Params;
use crate::{Error, Result, each_row_mut};

/// How many distinct depths the source could express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitDepth {
    Eight,
    Sixteen,
}

impl BitDepth {
    /// Number of representable levels.
    pub fn levels(self) -> u32 {
        match self {
            BitDepth::Eight => 256,
            BitDepth::Sixteen => 65_536,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BitDepth::Eight => "8-bit",
            BitDepth::Sixteen => "16-bit",
        }
    }
}

/// A decoded depth map: one f32 in `0.0..=1.0` per pixel, row-major.
#[derive(Debug, Clone)]
pub struct DepthMap {
    pub width: u32,
    pub height: u32,
    pub bits: BitDepth,
    /// True when the source had colour, so the luma conversion was a guess.
    pub was_colour: bool,
    pub data: Vec<f32>,
}

impl DepthMap {
    /// Decode PNG (8- or 16-bit) or JPEG from memory.
    ///
    /// Colour inputs are converted with Rec. 601 luma weights and flagged, so
    /// the UI can say out loud that a colour image is not really a depth map.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let image = image::load_from_memory(bytes)?;
        Self::from_dynamic(&image)
    }

    pub fn from_dynamic(image: &image::DynamicImage) -> Result<Self> {
        use image::DynamicImage as D;

        let (width, height) = (image.width(), image.height());
        if width < 2 || height < 2 {
            return Err(Error::TooSmall { width, height });
        }

        let bits = match image {
            D::ImageLuma16(_) | D::ImageLumaA16(_) | D::ImageRgb16(_) | D::ImageRgba16(_) => {
                BitDepth::Sixteen
            }
            _ => BitDepth::Eight,
        };
        let was_colour = !matches!(
            image,
            D::ImageLuma8(_) | D::ImageLumaA8(_) | D::ImageLuma16(_) | D::ImageLumaA16(_)
        );

        // to_luma16 applies Rec. 601 weights for colour sources and widens
        // 8-bit ones, so one path covers every accepted format. The 8-bit
        // widening is exact (v * 257), and `bits` remembers the true source
        // depth for the step-size check.
        let luma = image.to_luma16();
        let scale = 1.0 / u16::MAX as f32;
        let data = luma.as_raw().iter().map(|&v| v as f32 * scale).collect();

        Ok(Self {
            width,
            height,
            bits,
            was_colour,
            data,
        })
    }

    pub fn aspect(&self) -> f32 {
        self.width as f32 / self.height as f32
    }

    fn at(&self, x: usize, y: usize) -> f32 {
        let x = x.min(self.width as usize - 1);
        let y = y.min(self.height as usize - 1);
        self.data[y * self.width as usize + x]
    }

    /// Bilinear resample to `(w, h)`.
    ///
    /// In f32 throughout. Upstream round-trips PIL's `F → I` here, quantising
    /// back to integers between the resize and the mesh; there is no reason to
    /// throw that precision away.
    pub fn resized(&self, w: usize, h: usize) -> Self {
        let (w, h) = (w.max(2), h.max(2));
        if w == self.width as usize && h == self.height as usize {
            return self.clone();
        }

        let mut out = vec![0.0f32; w * h];
        // Map destination centres onto source centres: the -1/-1 form puts the
        // first and last destination samples exactly on the first and last
        // source pixels, which keeps the relief's edges where they were.
        let sx = (self.width as f32 - 1.0) / (w as f32 - 1.0);
        let sy = (self.height as f32 - 1.0) / (h as f32 - 1.0);

        each_row_mut(&mut out, w, |j, row| {
            let fy = j as f32 * sy;
            let y0 = fy.floor() as usize;
            let ty = fy - y0 as f32;
            for (i, slot) in row.iter_mut().enumerate() {
                let fx = i as f32 * sx;
                let x0 = fx.floor() as usize;
                let tx = fx - x0 as f32;
                let a = self.at(x0, y0);
                let b = self.at(x0 + 1, y0);
                let c = self.at(x0, y0 + 1);
                let d = self.at(x0 + 1, y0 + 1);
                let top = a + (b - a) * tx;
                let bottom = c + (d - c) * tx;
                *slot = top + (bottom - top) * ty;
            }
        });

        Self {
            width: w as u32,
            height: h as u32,
            bits: self.bits,
            was_colour: self.was_colour,
            data: out,
        }
    }

    /// Grid dimensions this map would produce under `params`.
    pub fn grid(&self, params: &Params) -> (usize, usize) {
        params.grid_for(self.width, self.height)
    }

    /// Decode, resample and condition in one step: the whole front half of the
    /// pipeline, from file bytes to something meshable.
    pub fn to_height_field(&self, params: &Params) -> Result<HeightField> {
        params.validate()?;
        let (w, h) = self.grid(params);
        Ok(self.resized(w, h).condition(params))
    }

    /// Median filter, normalise, window, emboss — upstream's six lines of numpy.
    ///
    /// Order matters and matches `extrude.py:136-146`: the filter runs on the
    /// raw values (so it sees real depth spikes), normalisation stretches
    /// whatever survives to `0..1`, and only then is the near/far window applied
    /// and rescaled to the emboss depth. Inversion happens first, where it is
    /// equivalent to inverting the source file.
    pub fn condition(&self, params: &Params) -> HeightField {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut v = self.data.clone();

        if params.invert {
            for x in &mut v {
                *x = 1.0 - *x;
            }
        }
        // 8-bit only: 16-bit input has no staircase worth breaking, and blurring
        // it would only cost detail.
        if params.presmooth > 0.0 && self.bits == BitDepth::Eight {
            v = gaussian(&v, w, h, params.presmooth);
        }
        if params.filter_size > 1 {
            v = median(&v, w, h, params.filter_size as usize);
        }

        let (lo, hi) = v
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &x| {
                (lo.min(x), hi.max(x))
            });
        let span = if hi > lo { hi - lo } else { 1.0 };
        let window = params.far - params.near;

        let mut z = v;
        for x in &mut z {
            let t = ((*x - lo) / span).clamp(params.near, params.far);
            // Millimetres, and negative: dark/near sits at 0 and bright recedes
            // to the full emboss height.
            *x = -params.emboss_mm * (t - params.near) / window;
        }

        HeightField {
            w,
            h,
            z,
            bits: self.bits,
        }
    }
}

/// A conditioned height field: `z` in millimetres, `-emboss_mm..=0`.
///
/// Zero is the front of the relief and the values run negative into it, which is
/// the sign convention the whole crate uses. [`crate::mesh::Layout`] supplies the
/// x and y spacing that goes with it.
#[derive(Debug, Clone)]
pub struct HeightField {
    pub w: usize,
    pub h: usize,
    pub z: Vec<f32>,
    /// Carried through from the source so the step-size check can use it.
    pub bits: BitDepth,
}

impl HeightField {
    #[inline]
    pub fn at(&self, i: usize, j: usize) -> f32 {
        self.z[j * self.w + i]
    }

    /// Sample with edge clamping, for difference stencils on the border.
    #[inline]
    pub fn at_clamped(&self, i: isize, j: isize) -> f32 {
        let i = i.clamp(0, self.w as isize - 1) as usize;
        let j = j.clamp(0, self.h as isize - 1) as usize;
        self.at(i, j)
    }

    /// Deepest point, as a positive depth.
    pub fn depth(&self) -> f32 {
        -self.z.iter().copied().fold(0.0f32, f32::min)
    }
}

/// A photo to colour the relief with, resampled onto the mesh grid.
#[derive(Debug, Clone)]
pub struct Albedo {
    pub w: usize,
    pub h: usize,
    /// RGB, one triple per grid vertex.
    pub rgb: Vec<[u8; 3]>,
}

impl Albedo {
    /// Decode and resample a photo onto a `w × h` grid.
    ///
    /// The caller has already checked the aspect with [`check_aspect`]; this
    /// stretches to fit whatever it is given, because refusing here would mean
    /// discarding the depth map too.
    pub fn decode(bytes: &[u8], w: usize, h: usize) -> Result<Self> {
        let image = image::load_from_memory(bytes)?;
        Ok(Self::from_dynamic(&image, w, h))
    }

    pub fn from_dynamic(image: &image::DynamicImage, w: usize, h: usize) -> Self {
        let scaled = image.resize_exact(
            w.max(1) as u32,
            h.max(1) as u32,
            image::imageops::FilterType::Lanczos3,
        );
        let rgb = scaled.to_rgb8();
        Self {
            w,
            h,
            rgb: rgb
                .as_raw()
                .chunks_exact(3)
                .map(|c| [c[0], c[1], c[2]])
                .collect(),
        }
    }

    #[inline]
    pub fn at(&self, i: usize, j: usize) -> [u8; 3] {
        let i = i.min(self.w.saturating_sub(1));
        let j = j.min(self.h.saturating_sub(1));
        self.rgb
            .get(j * self.w + i)
            .copied()
            .unwrap_or([0x80, 0x80, 0x80])
    }
}

/// Refuse a photo whose shape cannot line up with the depth map.
///
/// Upstream means to do this at `app.py:80-86` but compares
/// `depth.size[0] * rgb.size[1]` with the identical expression, so the check
/// never fires. A 2% tolerance covers the off-by-one that a depth model's own
/// resampling introduces, and nothing wider.
pub fn check_aspect(depth: &DepthMap, photo_w: u32, photo_h: u32) -> Result<()> {
    let want = depth.aspect();
    let got = photo_w as f32 / photo_h as f32;
    if (want - got).abs() > 0.02 * want {
        return Err(Error::AspectMismatch {
            photo_w,
            photo_h,
            depth_w: depth.width,
            depth_h: depth.height,
        });
    }
    Ok(())
}

/// Median filter with a `size × size` window and clamped edges.
///
/// Median rather than Gaussian, as upstream: it removes isolated depth spikes
/// and JPEG ringing without rounding off the real edges that give a relief its
/// definition. `size` is odd and at most 5, so a 25-element insertion sort in a
/// stack array beats any cleverer algorithm at this scale.
fn median(src: &[f32], w: usize, h: usize, size: usize) -> Vec<f32> {
    let r = (size / 2) as isize;
    let mut out = vec![0.0f32; w * h];

    each_row_mut(&mut out, w, |j, row| {
        let mut window = [0.0f32; 25];
        for (i, slot) in row.iter_mut().enumerate() {
            let mut n = 0;
            for dj in -r..=r {
                let y = (j as isize + dj).clamp(0, h as isize - 1) as usize;
                for di in -r..=r {
                    let x = (i as isize + di).clamp(0, w as isize - 1) as usize;
                    let v = src[y * w + x];
                    // Insertion sort as we gather, so no separate sort pass.
                    let mut k = n;
                    while k > 0 && window[k - 1] > v {
                        window[k] = window[k - 1];
                        k -= 1;
                    }
                    window[k] = v;
                    n += 1;
                }
            }
            *slot = window[n / 2];
        }
    });
    out
}

/// Separable Gaussian blur with clamped edges.
fn gaussian(src: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let radius = (sigma * 3.0).ceil().max(1.0) as isize;
    let kernel: Vec<f32> = (-radius..=radius)
        .map(|d| {
            let x = d as f32 / sigma;
            (-0.5 * x * x).exp()
        })
        .collect();
    let norm: f32 = kernel.iter().sum();

    let mut pass = vec![0.0f32; w * h];
    each_row_mut(&mut pass, w, |j, row| {
        for (i, slot) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (k, weight) in kernel.iter().enumerate() {
                let x = (i as isize + k as isize - radius).clamp(0, w as isize - 1) as usize;
                acc += weight * src[j * w + x];
            }
            *slot = acc / norm;
        }
    });

    let mut out = vec![0.0f32; w * h];
    each_row_mut(&mut out, w, |j, row| {
        for (i, slot) in row.iter_mut().enumerate() {
            let mut acc = 0.0;
            for (k, weight) in kernel.iter().enumerate() {
                let y = (j as isize + k as isize - radius).clamp(0, h as isize - 1) as usize;
                acc += weight * pass[y * w + i];
            }
            *slot = acc / norm;
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(w: usize, h: usize) -> DepthMap {
        let data = (0..w * h)
            .map(|k| (k % w) as f32 / (w - 1) as f32)
            .collect();
        DepthMap {
            width: w as u32,
            height: h as u32,
            bits: BitDepth::Sixteen,
            was_colour: false,
            data,
        }
    }

    #[test]
    fn median_removes_a_spike_but_keeps_an_edge() {
        let w = 7;
        let h = 7;
        let mut v = vec![0.2f32; w * h];
        v[3 * w + 3] = 1.0; // lone spike
        let out = median(&v, w, h, 3);
        assert!((out[3 * w + 3] - 0.2).abs() < 1e-6, "spike survived");

        // A step edge must stay a step edge.
        let mut e = vec![0.0f32; w * h];
        for j in 0..h {
            for i in 0..w {
                e[j * w + i] = if i >= 4 { 1.0 } else { 0.0 };
            }
        }
        let out = median(&e, w, h, 3);
        assert_eq!(out[3 * w + 2], 0.0);
        assert_eq!(out[3 * w + 5], 1.0);
    }

    #[test]
    fn conditioning_spans_exactly_the_emboss_depth() {
        let p = Params {
            emboss_mm: 40.0,
            filter_size: 1,
            ..Params::default()
        };
        let field = ramp(9, 5).condition(&p);
        let lo = field.z.iter().copied().fold(f32::INFINITY, f32::min);
        let hi = field.z.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        assert!((hi - 0.0).abs() < 1e-6, "near side should sit at z = 0");
        assert!(
            (lo + 40.0).abs() < 1e-4,
            "far side should sit at -emboss_mm"
        );
    }

    #[test]
    fn window_flattens_everything_outside_it() {
        let p = Params {
            near: 0.25,
            far: 0.75,
            emboss_mm: 25.0,
            filter_size: 1,
            ..Params::default()
        };
        let field = ramp(101, 3).condition(&p);
        // Columns below the near plane are all one plateau, as are those above far.
        assert_eq!(field.at(0, 0), field.at(20, 0));
        assert_eq!(field.at(80, 0), field.at(100, 0));
        assert!(field.at(50, 0) < field.at(20, 0));
    }

    #[test]
    fn invert_mirrors_the_relief() {
        let p = Params {
            filter_size: 1,
            ..Params::default()
        };
        let straight = ramp(9, 3).condition(&p);
        let flipped = ramp(9, 3).condition(&Params { invert: true, ..p });
        for i in 0..9 {
            assert!((straight.at(i, 0) - flipped.at(8 - i, 0)).abs() < 1e-6);
        }
    }

    #[test]
    fn resize_preserves_the_corners() {
        let src = ramp(9, 9);
        let out = src.resized(5, 5);
        assert!((out.at(0, 0) - 0.0).abs() < 1e-6);
        assert!((out.at(4, 0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn aspect_mismatch_is_caught() {
        let depth = ramp(1536, 1152);
        assert!(check_aspect(&depth, 1536, 1152).is_ok());
        assert!(check_aspect(&depth, 1024, 768).is_ok(), "same 4:3 is fine");
        assert!(check_aspect(&depth, 1000, 1000).is_err());
    }
}
