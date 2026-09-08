//! End to end: image bytes in, mesh bytes out.
//!
//! The unit tests inside the crate check each stage. These check that the stages
//! still fit together, and that the two claims a user relies on hold on a real
//! file: the mesh is printable, and the same settings produce the same bytes.

use relief_core::depth::{Albedo, BitDepth, DepthMap};
use relief_core::export::{self, Format};
use relief_core::params::{Orientation, Params};
use relief_core::{Solid, Stats, check, sample};

fn png(bits: BitDepth) -> Vec<u8> {
    sample::to_png(&sample::depth_map_with(384, 288, bits)).expect("sample encodes")
}

fn params() -> Params {
    Params {
        size_px: 256,
        emboss_mm: 30.0,
        size_mm: 120.0,
        ..Params::default()
    }
}

#[test]
fn sixteen_bit_png_to_watertight_stl() {
    let bytes = png(BitDepth::Sixteen);
    let map = DepthMap::decode(&bytes).expect("decodes");
    assert_eq!(map.bits, BitDepth::Sixteen);
    assert!(!map.was_colour);

    let params = params();
    let field = map.to_height_field(&params).expect("conditions");
    assert_eq!((field.w, field.h), (256, 192));

    let solid = Solid::new(&field, &params);
    let report = check::inspect(&solid);
    assert_eq!(report.watertight(), Some(true), "{report:?}");

    let stl = export::to_vec(Format::Stl, &solid).expect("writes");
    assert_eq!(stl.len(), 84 + 50 * solid.triangle_count());
    assert_eq!(&stl[..12], b"relief-forge");

    // The count in the header is what a slicer trusts.
    let declared = u32::from_le_bytes(stl[80..84].try_into().unwrap()) as usize;
    assert_eq!(declared, solid.triangle_count());
}

#[test]
fn eight_bit_input_is_accepted_and_reported_honestly() {
    let bytes = png(BitDepth::Eight);
    let map = DepthMap::decode(&bytes).expect("decodes");
    assert_eq!(map.bits, BitDepth::Eight);

    // At this size the 8-bit step is comfortably under a 0.1 mm layer, which is
    // the whole argument for accepting 8-bit at all.
    let params = Params {
        size_mm: 100.0,
        emboss_mm: 20.0,
        ..params()
    };
    let field = map.to_height_field(&params).expect("conditions");
    let solid = Solid::new(&field, &params);
    let stats = Stats::new(&solid, Format::Stl, &params);
    assert_eq!(stats.levels, 256);
    assert!(stats.steps_ok(), "{}", stats.step_line());

    // Deepen the relief far enough and the same 256 levels stop being enough.
    let deep = Params {
        size_mm: 300.0,
        emboss_mm: 120.0,
        ..params
    };
    let field = map.to_height_field(&deep).expect("conditions");
    let solid = Solid::new(&field, &deep);
    let stats = Stats::new(&solid, Format::Stl, &deep);
    assert!(!stats.steps_ok(), "{}", stats.step_line());
    assert!(stats.step_line().contains("16-bit"), "advice is missing");
}

#[test]
fn pre_smoothing_calms_an_eight_bit_source() {
    let bytes = png(BitDepth::Eight);
    let map = DepthMap::decode(&bytes).expect("decodes");

    // Curvature, not slope: the second difference measures how much the field
    // *jitters* between neighbours, while ignoring the honest gradients that a
    // relief is made of. Quantisation steps and JPEG blocks are curvature.
    let roughness = |presmooth: f32| {
        let params = Params {
            filter_size: 1,
            presmooth,
            ..params()
        };
        let field = map.to_height_field(&params).expect("conditions");
        let mut total = 0.0f64;
        for j in 0..field.h {
            for i in 1..field.w - 1 {
                let curvature = field.at(i - 1, j) - 2.0 * field.at(i, j) + field.at(i + 1, j);
                total += curvature.abs() as f64;
            }
        }
        total
    };

    let raw = roughness(0.0);
    let smoothed = roughness(1.5);
    assert!(
        smoothed < raw * 0.5,
        "pre-smoothing barely helped: {raw} -> {smoothed}"
    );
}

#[test]
fn a_photo_colours_the_relief_but_never_the_frame() {
    let depth_bytes = png(BitDepth::Sixteen);
    let map = DepthMap::decode(&depth_bytes).expect("decodes");
    let params = params();
    let field = map.to_height_field(&params).expect("conditions");

    // A photo of the same shape, solid red.
    let photo = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        384,
        288,
        image::Rgb([255, 0, 0]),
    ));
    let albedo = Albedo::from_dynamic(&photo, field.w, field.h);

    let solid = Solid::new(&field, &params).with_albedo(Some(&albedo));
    assert!(solid.has_colour());
    assert_eq!(
        solid.colour(0),
        [255, 0, 0],
        "surface should take the photo"
    );
    let frame_vertex = solid.vertex_count() - 1; // the back plate's centre
    assert_eq!(
        solid.colour(frame_vertex),
        relief_core::mesh::FRAME_GREY,
        "the frame is not part of the picture"
    );

    // PLY carries it; STL structurally cannot.
    assert!(Format::Ply.carries_colour());
    assert!(!Format::Stl.carries_colour());
    let ply = export::to_vec(Format::Ply, &solid).expect("writes");
    let header_end = ply.windows(11).position(|w| w == b"end_header\n").unwrap() + 11;
    let header = String::from_utf8_lossy(&ply[..header_end]);
    assert!(header.contains("property uchar red"), "{header}");
    // First vertex: 12 bytes of position then the colour.
    assert_eq!(&ply[header_end + 12..header_end + 15], &[255, 0, 0]);
}

#[test]
fn the_same_settings_produce_the_same_bytes() {
    let bytes = png(BitDepth::Sixteen);
    let map = DepthMap::decode(&bytes).expect("decodes");
    let params = params();

    let once = {
        let field = map.to_height_field(&params).expect("conditions");
        export::to_vec(Format::Stl, &Solid::new(&field, &params)).expect("writes")
    };
    let twice = {
        let field = map.to_height_field(&params).expect("conditions");
        export::to_vec(Format::Stl, &Solid::new(&field, &params)).expect("writes")
    };
    assert_eq!(once, twice, "export is not deterministic");
}

#[test]
fn print_orientation_changes_the_axes_but_not_the_shape() {
    let bytes = png(BitDepth::Sixteen);
    let map = DepthMap::decode(&bytes).expect("decodes");

    let mut extents = Vec::new();
    for orientation in Orientation::ALL {
        let params = Params {
            orientation,
            ..params()
        };
        let field = map.to_height_field(&params).expect("conditions");
        let solid = Solid::new(&field, &params);
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for v in 0..solid.vertex_count() {
            let p = solid.print_position(v).to_array();
            for axis in 0..3 {
                min[axis] = min[axis].min(p[axis]);
                max[axis] = max[axis].max(p[axis]);
            }
        }
        // Whatever the orientation, the model rests on the bed.
        assert!(min[2].abs() < 1e-3, "{orientation:?} floats at {}", min[2]);
        let mut size = [0.0f32; 3];
        for axis in 0..3 {
            size[axis] = max[axis] - min[axis];
        }
        extents.push(size);
    }

    // Face up is wide and shallow; upright puts the relief depth on Y and the
    // image's height on Z. Same three numbers, permuted.
    let mut a = extents[0];
    let mut b = extents[1];
    a.sort_by(|x, y| x.partial_cmp(y).unwrap());
    b.sort_by(|x, y| x.partial_cmp(y).unwrap());
    for axis in 0..3 {
        assert!(
            (a[axis] - b[axis]).abs() < 1e-2,
            "orientation changed the shape: {a:?} vs {b:?}"
        );
    }
    assert!(
        extents[0][2] < extents[1][2],
        "upright should be the taller one"
    );
}

#[test]
fn empty_depth_window_is_refused_with_a_readable_message() {
    let bytes = png(BitDepth::Sixteen);
    let map = DepthMap::decode(&bytes).expect("decodes");
    let params = Params {
        near: 0.7,
        far: 0.3,
        ..params()
    };
    let error = map.to_height_field(&params).expect_err("should refuse");
    let text = error.to_string();
    assert!(text.contains("near plane"), "{text}");
    assert!(text.contains("nothing is left between them"), "{text}");
}

#[test]
fn junk_bytes_do_not_panic() {
    assert!(DepthMap::decode(b"not an image at all").is_err());
    assert!(DepthMap::decode(&[]).is_err());
}

/// The convention this whole pipeline turns on, pinned with a test.
///
/// A *bright* pixel is far away — that is what upstream's `z = -emboss * value`
/// means, and every default here follows it. So the sample's massif has to be
/// the dark part of its image, and the geometry has to put the middle of the
/// plaque at the front. Get this backwards and every relief comes out as a
/// mould of itself.
#[test]
fn the_sample_stands_proud_of_its_plate() {
    let bytes = png(BitDepth::Sixteen);
    let map = DepthMap::decode(&bytes).expect("decodes");
    let params = params();

    // In the source image, the middle is dark and the corner is bright.
    let middle_px =
        map.data[(map.height / 2) as usize * map.width as usize + (map.width / 2) as usize];
    let corner_px = map.data[0];
    assert!(
        middle_px < 0.35 && corner_px > 0.8,
        "sample convention changed: middle {middle_px}, corner {corner_px}"
    );

    // In the geometry, the middle is at the front and the corner at the back.
    let field = map.to_height_field(&params).expect("conditions");
    let middle = field.at(field.w / 2, field.h / 2);
    let corner = field.at(0, 0);
    // Within a few percent of the front: the exact centre pixel is not the
    // global minimum of a noisy dome, and does not need to be.
    assert!(
        middle > -0.05 * params.emboss_mm,
        "the middle should sit at the front, not {middle}"
    );
    assert!(
        (corner + params.emboss_mm).abs() < 0.02 * params.emboss_mm,
        "the corner should sit at the back, not {corner}"
    );

    // And inverting turns it inside out, which is the point of the control.
    let flipped = map
        .to_height_field(&Params {
            invert: true,
            ..params
        })
        .expect("conditions");
    assert!(flipped.at(flipped.w / 2, flipped.h / 2) < -0.9 * params.emboss_mm);
    assert!(flipped.at(0, 0) > -0.02 * params.emboss_mm);
}

/// A zero-width frame is no frame — and still a printable solid.
#[test]
fn an_unframed_relief_is_a_solid_in_its_own_right() {
    let bytes = png(BitDepth::Sixteen);
    let map = DepthMap::decode(&bytes).expect("decodes");

    let framed = params();
    let bare = Params {
        frame_thickness_mm: 0.0,
        ..framed
    };

    let field = map.to_height_field(&bare).expect("conditions");
    let solid = Solid::new(&field, &bare);
    assert!(!solid.layout.framed());

    // Still closed, still genus 0, still wound outwards.
    let report = check::inspect(&solid);
    assert_eq!(report.watertight(), Some(true), "{report:?}");

    // And genuinely smaller: two rings and two strips fewer, with no
    // zero-area bezel left behind.
    let framed_field = map.to_height_field(&framed).expect("conditions");
    let with_frame = Solid::new(&framed_field, &framed);
    assert!(
        solid.triangle_count() < with_frame.triangle_count(),
        "{} vs {}",
        solid.triangle_count(),
        with_frame.triangle_count()
    );
    assert!(
        export::to_vec(Format::Stl, &solid).unwrap().len()
            < export::to_vec(Format::Stl, &with_frame).unwrap().len()
    );

    // The plaque is exactly the requested size, with nothing standing proud.
    let e = solid.layout.extents_mm();
    assert!((e.x.max(e.y) - bare.size_mm).abs() < 1e-3, "{e}");
}
