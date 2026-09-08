//! Binary little-endian PLY.
//!
//! Indexed, so a vertex is stored once, and it carries per-vertex colour — the
//! two things STL cannot do. About a fifth of STL's size for the same mesh, and
//! the format to reach for when the photo matters.

use std::io::Write;

use crate::Result;
use crate::mesh::Solid;

pub fn write(solid: &Solid, out: &mut dyn Write, progress: &mut dyn FnMut(f32)) -> Result<()> {
    let colour = solid.has_colour();
    let vertices = solid.vertex_count();
    let triangles = solid.triangle_count();

    write!(
        out,
        "ply\nformat binary_little_endian 1.0\n\
         comment relief-forge\n\
         element vertex {vertices}\n\
         property float x\nproperty float y\nproperty float z\n"
    )?;
    if colour {
        out.write_all(b"property uchar red\nproperty uchar green\nproperty uchar blue\n")?;
    }
    write!(
        out,
        "element face {triangles}\n\
         property list uchar int vertex_indices\n\
         end_header\n"
    )?;

    // Vertices, in index order. `print_position` is O(1) per vertex, so this
    // walk allocates nothing.
    let mut record = [0u8; 15];
    let stride = if colour { 15 } else { 12 };
    let report_every = (vertices / 200).max(1);
    for v in 0..vertices {
        let p = solid.print_position(v);
        record[0..4].copy_from_slice(&p.x.to_le_bytes());
        record[4..8].copy_from_slice(&p.y.to_le_bytes());
        record[8..12].copy_from_slice(&p.z.to_le_bytes());
        if colour {
            record[12..15].copy_from_slice(&solid.colour(v));
        }
        out.write_all(&record[..stride])?;
        if v % report_every == 0 {
            progress(0.5 * v as f32 / vertices as f32);
        }
    }

    let mut face = [0u8; 13];
    face[0] = 3;
    let mut failed: Option<std::io::Error> = None;
    solid.for_each_triangle(
        |[a, b, c]| {
            if failed.is_some() {
                return;
            }
            face[1..5].copy_from_slice(&a.to_le_bytes());
            face[5..9].copy_from_slice(&b.to_le_bytes());
            face[9..13].copy_from_slice(&c.to_le_bytes());
            if let Err(e) = out.write_all(&face) {
                failed = Some(e);
            }
        },
        |t| progress(0.5 + 0.5 * t),
    );

    match failed {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}
