//! What simplification actually buys, on inputs of different quality.
//!
//! The measurement that decided the design, kept so it can be re-run: it drives
//! the real [`Decimation`], so these are exact triangle counts including the
//! stitching a restricted quadtree needs, not an estimate.
//!
//! The finding that shaped the control: **a tolerance below the source's
//! quantisation step buys nothing**, because every step is then a feature that
//! has to be preserved. An 8-bit map over 20 mm of relief steps every 0.078 mm,
//! which is why the 0.05 mm column collapses for 8-bit input and the 0.10 mm one
//! does not — and why `Params::presmooth` is as much a simplification control as
//! a quality one.
//!
//! Run with `cargo run -p relief-core --example decimation_probe --release`.

use relief_core::decimate::Decimation;
use relief_core::depth::BitDepth;
use relief_core::params::Params;
use relief_core::sample;

const SIDE: u32 = 1025;
const TOLERANCES: [f32; 4] = [0.02, 0.05, 0.1, 0.2];

fn main() {
    let full = 2 * (SIDE as usize - 1) * (SIDE as usize - 1);
    println!("A {SIDE}×{SIDE} grid is {full} triangles at full resolution.");
    println!("100 mm plaque, 20 mm of relief: an 8-bit source steps every 0.078 mm.\n");

    print!("{:<26}", "input");
    for tolerance in TOLERANCES {
        print!("{:>14}", format!("{tolerance:.2} mm"));
    }
    println!();

    for (label, bits, filter, presmooth) in [
        ("16-bit, median 3px", BitDepth::Sixteen, 3, 0.0),
        ("16-bit, unfiltered", BitDepth::Sixteen, 1, 0.0),
        ("8-bit, unfiltered", BitDepth::Eight, 1, 0.0),
        ("8-bit + JPEG, median 3px", BitDepth::Eight, 3, 0.0),
        ("8-bit + pre-smooth 1px", BitDepth::Eight, 3, 1.0),
        ("8-bit + pre-smooth 2px", BitDepth::Eight, 3, 2.0),
    ] {
        let params = Params {
            size_px: SIDE,
            size_mm: 100.0,
            emboss_mm: 20.0,
            filter_size: filter,
            presmooth,
            frame_thickness_mm: 0.0,
            ..Params::default()
        };
        let field = sample::depth_map_with(2048, 2048, bits)
            .to_height_field(&params)
            .expect("the sample conditions cleanly");

        print!("{label:<26}");
        for tolerance in TOLERANCES {
            let d = Decimation::build(&field, tolerance).expect("simplification is on");
            print!(
                "{:>13}",
                format!("{:.1}%", 100.0 * d.triangles() as f64 / full as f64)
            );
            print!(" ");
        }
        println!();
    }

    println!("\nPercentage of the full-resolution triangles that survive: lower is better.");
}
