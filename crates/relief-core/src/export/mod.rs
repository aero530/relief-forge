//! Streaming mesh writers.
//!
//! Every exporter here consumes a [`Solid`] and an [`std::io::Write`], and
//! never holds more than a few kilobytes of mesh at a time: vertex positions
//! are computed from their index on demand, and triangles are emitted in a
//! fixed order. Peak memory is therefore the height field plus the writer's own
//! buffer, whatever the resolution — a 4096-px relief writes a 1.26 GB STL
//! through a 1 MB buffer.
//!
//! That is what makes the browser build work: on wasm the sink accumulates
//! fixed-size chunks and hands each one to a JS `Blob`, so a 300 MB export
//! never has to exist inside the 32-bit heap.
//!
//! # Choosing a format
//!
//! | Format | Colour | Size per triangle | Notes |
//! |--------|--------|-------------------|-------|
//! | STL    | no     | 50 bytes          | What every slicer wants. No indices, so 3× the vertex data. |
//! | PLY    | yes    | ~13 bytes + verts | Binary, indexed, carries vertex colour. Best of the three. |
//! | OBJ    | yes    | ~24 bytes + verts | ASCII, so the largest and slowest; colour is a widely-read extension. |

mod obj;
mod ply;
mod stl;

use std::io::{BufWriter, Write};

use crate::Result;
use crate::mesh::Solid;

/// Size of the buffer wrapped around the caller's writer.
const BUFFER: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Stl,
    Obj,
    Ply,
}

impl Format {
    pub const ALL: [Format; 3] = [Format::Stl, Format::Ply, Format::Obj];

    pub fn extension(self) -> &'static str {
        match self {
            Format::Stl => "stl",
            Format::Obj => "obj",
            Format::Ply => "ply",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Stl => "STL",
            Format::Obj => "OBJ",
            Format::Ply => "PLY",
        }
    }

    /// Whether the format can carry the photo's colours.
    pub fn carries_colour(self) -> bool {
        match self {
            Format::Stl => false,
            Format::Obj | Format::Ply => true,
        }
    }

    /// Bytes the file will take, near enough to warn on.
    ///
    /// Exact for STL and PLY, which are binary and fixed-width. OBJ is ASCII,
    /// so its estimate assumes typical coordinate lengths.
    pub fn estimated_bytes(self, solid: &Solid) -> u64 {
        let v = solid.vertex_count() as u64;
        let t = solid.triangle_count() as u64;
        let colour = solid.has_colour() && self.carries_colour();
        match self {
            Format::Stl => 84 + 50 * t,
            Format::Ply => 300 + v * if colour { 15 } else { 12 } + t * 13,
            Format::Obj => 100 + v * if colour { 62 } else { 38 } + t * 26,
        }
    }
}

/// Write `solid` to `out` in `format`.
///
/// `progress` is called with a fraction in `0.0..=1.0` a few hundred times over
/// the course of the write — often enough for a progress line, rarely enough to
/// cost nothing.
pub fn write(
    format: Format,
    solid: &Solid,
    out: &mut dyn Write,
    progress: &mut dyn FnMut(f32),
) -> Result<()> {
    // Buffered here rather than at the call site so no caller can accidentally
    // issue one syscall per 50-byte facet.
    let mut out = BufWriter::with_capacity(BUFFER, out);
    match format {
        Format::Stl => stl::write(solid, &mut out, progress)?,
        Format::Obj => obj::write(solid, &mut out, progress)?,
        Format::Ply => ply::write(solid, &mut out, progress)?,
    }
    out.flush()?;
    Ok(())
}

/// Convenience for tests and the CLI: the whole file in memory.
pub fn to_vec(format: Format, solid: &Solid) -> Result<Vec<u8>> {
    let mut buffer = Vec::with_capacity(format.estimated_bytes(solid) as usize);
    write(format, solid, &mut buffer, &mut |_| {})?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::depth::{BitDepth, HeightField};
    use crate::params::Params;

    fn solid_fixture() -> (HeightField, Params) {
        let (w, h) = (9usize, 5usize);
        let z = (0..w * h)
            .map(|k| {
                let (i, j) = (k % w, k / w);
                -0.15 * ((i as f32 / 8.0) + (j as f32 / 4.0)) / 2.0
            })
            .collect();
        (
            HeightField {
                w,
                h,
                z,
                bits: BitDepth::Sixteen,
            },
            Params::default(),
        )
    }

    #[test]
    fn stl_size_is_exactly_what_the_estimate_says() {
        let (field, params) = solid_fixture();
        let solid = Solid::new(&field, &params);
        let bytes = to_vec(Format::Stl, &solid).unwrap();
        assert_eq!(bytes.len() as u64, Format::Stl.estimated_bytes(&solid));

        // The count in the header must match the body.
        let count = u32::from_le_bytes(bytes[80..84].try_into().unwrap());
        assert_eq!(count as usize, solid.triangle_count());
        assert_eq!(bytes.len(), 84 + 50 * count as usize);
    }

    #[test]
    fn ply_size_is_exactly_what_the_estimate_says() {
        let (field, params) = solid_fixture();
        let solid = Solid::new(&field, &params);
        let bytes = to_vec(Format::Ply, &solid).unwrap();
        // The header is the only variable-length part, so compare the body.
        let header_end = bytes
            .windows(11)
            .position(|w| w == b"end_header\n")
            .unwrap()
            + 11;
        let body = bytes.len() - header_end;
        let expect = solid.vertex_count() * 12 + solid.triangle_count() * 13;
        assert_eq!(body, expect);
    }

    #[test]
    fn obj_round_trips_its_own_counts() {
        let (field, params) = solid_fixture();
        let solid = Solid::new(&field, &params);
        let text = String::from_utf8(to_vec(Format::Obj, &solid).unwrap()).unwrap();
        let vs = text.lines().filter(|l| l.starts_with("v ")).count();
        let fs = text.lines().filter(|l| l.starts_with("f ")).count();
        assert_eq!(vs, solid.vertex_count());
        assert_eq!(fs, solid.triangle_count());
        // Indices are 1-based and in range.
        for line in text.lines().filter(|l| l.starts_with("f ")) {
            for index in line[2..].split_whitespace() {
                let index: usize = index.parse().unwrap();
                assert!(index >= 1 && index <= vs, "index {index} out of range");
            }
        }
    }

    #[test]
    fn progress_ends_at_one() {
        let (field, params) = solid_fixture();
        let solid = Solid::new(&field, &params);
        for format in Format::ALL {
            let mut last = 0.0;
            let mut sink = Vec::new();
            write(format, &solid, &mut sink, &mut |t| {
                assert!((0.0..=1.0).contains(&t), "{format:?} reported {t}");
                last = t;
            })
            .unwrap();
            assert!(
                (last - 1.0).abs() < 1e-6,
                "{format:?} finished at {last}, not 1.0"
            );
        }
    }
}
