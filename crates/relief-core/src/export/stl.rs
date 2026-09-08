//! Binary STL.
//!
//! 50 bytes per triangle — a normal, three vertices, and a two-byte attribute
//! field nobody reads — with no index buffer, so the same vertex is written once
//! per triangle that touches it. Wasteful, and still the only format every
//! slicer is guaranteed to open.

use std::io::Write;

use crate::Result;
use crate::mesh::Solid;

const HEADER: &[u8] = b"relief-forge binary STL";

pub fn write(solid: &Solid, out: &mut dyn Write, progress: &mut dyn FnMut(f32)) -> Result<()> {
    let mut header = [0u8; 80];
    header[..HEADER.len()].copy_from_slice(HEADER);
    out.write_all(&header)?;
    out.write_all(&(solid.triangle_count() as u32).to_le_bytes())?;

    let mut facet = [0u8; 50];
    let mut failed: Option<std::io::Error> = None;

    solid.for_each_facet(
        |tri, normal| {
            if failed.is_some() {
                return;
            }
            let mut at = 0;
            for value in [normal.x, normal.y, normal.z]
                .into_iter()
                .chain(tri.iter().flat_map(|v| [v.x, v.y, v.z]))
            {
                facet[at..at + 4].copy_from_slice(&value.to_le_bytes());
                at += 4;
            }
            // Attribute byte count: zero. Some tools smuggle colour in here;
            // no slicer agrees on how, so it stays zero and colour goes to PLY.
            facet[48] = 0;
            facet[49] = 0;
            if let Err(e) = out.write_all(&facet) {
                failed = Some(e);
            }
        },
        progress,
    );

    match failed {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}
