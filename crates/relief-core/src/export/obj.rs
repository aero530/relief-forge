//! Wavefront OBJ.
//!
//! ASCII, so it is the biggest and slowest of the three by a wide margin — kept
//! because it is the format every mesh tool will open, and because it can carry
//! colour as the `v x y z r g b` extension MeshLab and Blender both read.
//!
//! Vertex normals are deliberately not written. Slicers ignore them, mesh tools
//! recompute them, and they would add half again to the file for nothing.

use std::io::Write;

use crate::Result;
use crate::mesh::Solid;

pub fn write(solid: &Solid, out: &mut dyn Write, progress: &mut dyn FnMut(f32)) -> Result<()> {
    let colour = solid.has_colour();
    let vertices = solid.vertex_count();
    let triangles = solid.triangle_count();

    writeln!(out, "# relief-forge")?;
    writeln!(
        out,
        "# {vertices} vertices, {triangles} triangles, millimetres"
    )?;

    let report_every = (vertices / 200).max(1);
    for v in 0..vertices {
        let p = solid.print_position(v);
        if colour {
            let [r, g, b] = solid.colour(v);
            let f = |c: u8| c as f32 / 255.0;
            writeln!(
                out,
                "v {:.4} {:.4} {:.4} {:.4} {:.4} {:.4}",
                p.x,
                p.y,
                p.z,
                f(r),
                f(g),
                f(b)
            )?;
        } else {
            writeln!(out, "v {:.4} {:.4} {:.4}", p.x, p.y, p.z)?;
        }
        if v % report_every == 0 {
            progress(0.5 * v as f32 / vertices as f32);
        }
    }

    let mut failed: Option<std::io::Error> = None;
    solid.for_each_triangle(
        |[a, b, c]| {
            if failed.is_some() {
                return;
            }
            // OBJ indices are 1-based.
            if let Err(e) = writeln!(out, "f {} {} {}", a + 1, b + 1, c + 1) {
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
