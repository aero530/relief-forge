//! A synthetic depth map, so the app opens with something on screen.
//!
//! An app that starts empty shows nothing about what it does, and shipping a
//! photograph would mean shipping someone's likeness and a licence with it. This
//! is fractal terrain with a central massif: recognisably a height field, with
//! enough fine detail to show what the median filter does and a broad dome to
//! show what the near/far window does.
//!
//! It doubles as the fixture generator for the tests, and as `relief sample` on
//! the command line.

use crate::depth::{BitDepth, DepthMap};

/// Deterministic value hash. Not a good PRNG; it only has to be stable, so the
/// sample looks the same in the app, the tests and the docs.
fn hash(i: i32, j: i32) -> f32 {
    let mut n = i
        .wrapping_mul(374_761_393)
        .wrapping_add(j.wrapping_mul(668_265_263));
    n = (n ^ (n >> 13)).wrapping_mul(1_274_126_177);
    ((n ^ (n >> 16)) as u32) as f32 / u32::MAX as f32
}

/// Value noise with smoothstep interpolation.
fn value_noise(x: f32, y: f32) -> f32 {
    let (i, j) = (x.floor(), y.floor());
    let (fx, fy) = (x - i, y - j);
    let (i, j) = (i as i32, j as i32);
    let u = fx * fx * (3.0 - 2.0 * fx);
    let v = fy * fy * (3.0 - 2.0 * fy);
    let a = hash(i, j);
    let b = hash(i + 1, j);
    let c = hash(i, j + 1);
    let d = hash(i + 1, j + 1);
    a * (1.0 - u) * (1.0 - v) + b * u * (1.0 - v) + c * (1.0 - u) * v + d * u * v
}

/// The field itself, at normalised coordinates in `0..=1`.
///
/// A broad dome carrying fractal detail, on a flat background. The shape is
/// chosen to exercise the controls: the dome reads as a relief at a glance, the
/// detail is fine enough to show what the median filter does, and the flat
/// surround gives the near/far window something unambiguous to clip.
fn terrain(u: f32, v: f32) -> f32 {
    let mut amplitude = 0.5;
    let mut frequency = 4.0;
    let mut detail = 0.0;
    for _ in 0..5 {
        detail += amplitude * value_noise(u * frequency + 11.3, v * frequency + 4.7);
        amplitude *= 0.5;
        frequency *= 2.0;
    }

    let dx = (u - 0.5) * 2.15;
    let dy = (v - 0.5) * 2.6;
    let dome = (1.0 - (dx * dx + dy * dy)).clamp(0.0, 1.0).sqrt();

    // Detail rides on the dome rather than the background, so the plate stays
    // flat and the relief carries the texture.
    let height = dome * (0.72 + 0.28 * detail);

    // Inverted, because a depth map is not a height map: this pipeline (like
    // upstream) reads a *bright* pixel as far away, so the massif has to be the
    // dark part of the image for it to stand proud of the plate. Getting this
    // backwards is exactly what the Invert control is for on real files.
    1.0 - height.clamp(0.0, 1.0)
}

/// A 16-bit sample of the given size.
pub fn depth_map(width: u32, height: u32) -> DepthMap {
    depth_map_with(width, height, BitDepth::Sixteen)
}

/// A sample quantised to `bits`, for exercising the 8-bit path.
///
/// The 8-bit variant also gets 8×8 block ringing, which is what JPEG
/// compression actually does to a depth map and what the median filter is there
/// to remove — without it, an 8-bit sample would understate the problem.
pub fn depth_map_with(width: u32, height: u32, bits: BitDepth) -> DepthMap {
    let (w, h) = (width.max(2), height.max(2));
    let mut data = Vec::with_capacity((w * h) as usize);
    for j in 0..h {
        for i in 0..w {
            let u = i as f32 / (w - 1) as f32;
            let v = j as f32 / (h - 1) as f32;
            let mut value = terrain(u, v);
            if bits == BitDepth::Eight {
                value += (hash((i / 8) as i32 + 91, (j / 8) as i32 + 17) - 0.5) * 0.045;
                value += (hash(i as i32, j as i32) - 0.5) * 0.012;
                value = (value.clamp(0.0, 1.0) * 255.0).round() / 255.0;
            }
            data.push(value);
        }
    }
    DepthMap {
        width: w,
        height: h,
        bits,
        was_colour: false,
        data,
    }
}

/// Encode a depth map as a PNG, 16-bit or 8-bit to match its own bit depth.
pub fn to_png(map: &DepthMap) -> crate::Result<Vec<u8>> {
    use image::{DynamicImage, ImageFormat};

    let image = match map.bits {
        BitDepth::Sixteen => {
            let raw: Vec<u16> = map
                .data
                .iter()
                .map(|&v| (v.clamp(0.0, 1.0) * u16::MAX as f32).round() as u16)
                .collect();
            DynamicImage::ImageLuma16(
                image::ImageBuffer::from_raw(map.width, map.height, raw)
                    .expect("buffer is width * height"),
            )
        }
        BitDepth::Eight => {
            let raw: Vec<u8> = map
                .data
                .iter()
                .map(|&v| (v.clamp(0.0, 1.0) * 255.0).round() as u8)
                .collect();
            DynamicImage::ImageLuma8(
                image::ImageBuffer::from_raw(map.width, map.height, raw)
                    .expect("buffer is width * height"),
            )
        }
    };

    let mut bytes = Vec::new();
    image.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_uses_its_whole_range_and_is_stable() {
        let map = depth_map(64, 48);
        let lo = map.data.iter().copied().fold(f32::INFINITY, f32::min);
        let hi = map.data.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        // Not the full 0..1 — it does not need to be, because `condition`
        // normalises whatever range arrives. It does need enough spread that
        // the near/far window has something to bite on.
        assert!(lo < 0.2, "lowest sample is {lo}");
        assert!(hi > 0.6, "highest sample is {hi}");
        assert!(hi - lo > 0.5, "sample spans only {}", hi - lo);
        // Same seed, same terrain, every run.
        assert_eq!(map.data, depth_map(64, 48).data);
    }

    #[test]
    fn eight_bit_sample_is_quantised() {
        let map = depth_map_with(32, 32, BitDepth::Eight);
        assert_eq!(map.bits.levels(), 256);
        for &v in &map.data {
            let level = v * 255.0;
            assert!(
                (level - level.round()).abs() < 1e-4,
                "{v} is not on a 256-level grid"
            );
        }
    }

    #[test]
    fn png_round_trips_through_the_decoder() {
        let map = depth_map(48, 32);
        let png = to_png(&map).unwrap();
        let back = DepthMap::decode(&png).unwrap();
        assert_eq!((back.width, back.height), (48, 32));
        assert_eq!(back.bits, BitDepth::Sixteen);
        for (a, b) in map.data.iter().zip(&back.data) {
            assert!((a - b).abs() < 1.0 / 65_535.0 + 1e-6);
        }
    }
}
