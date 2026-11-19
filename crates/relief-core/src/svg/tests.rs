//! Fixtures in both dialects the drawings come from. No real file has been
//! supplied yet, so these are written to the shape each program actually
//! emits: Inkscape's labelled, millimetre-sized layers, and Illustrator's
//! `id`/`data-name` layers in points with a bare top-level object.

use super::*;
use crate::check;
use crate::params::Params;

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A 16-bit gray PNG: a horizontal ramp using levels an 8-bit file cannot.
fn ramp_png(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::ImageBuffer::<image::Luma<u16>, Vec<u16>>::new(w, h);
    for (x, _, p) in img.enumerate_pixels_mut() {
        *p = image::Luma([(x as f32 / (w - 1) as f32 * 65535.0) as u16]);
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageLuma16(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
    bytes
}

/// Inkscape: 100 × 60 mm, a photo layer and a *hidden* cutout layer holding a
/// circle and a separate rectangle.
fn inkscape(image_transform: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="no"?>
<svg width="100mm" height="60mm" viewBox="0 0 100 60" version="1.1"
   xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape"
   xmlns:sodipodi="http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd"
   xmlns:xlink="http://www.w3.org/1999/xlink"
   xmlns="http://www.w3.org/2000/svg">
  <sodipodi:namedview id="namedview1" inkscape:document-units="mm" />
  <defs id="defs1" />
  <g inkscape:label="Photo" inkscape:groupmode="layer" id="layer1">
    <image width="100" height="60" preserveAspectRatio="none" x="0" y="0" id="image1"
       {image_transform}
       xlink:href="data:image/png;base64,{png}" />
  </g>
  <g inkscape:groupmode="layer" id="layer2" inkscape:label="Cut" style="display:none">
    <circle style="fill:none;stroke:#ff0000;stroke-width:0.2" cx="30" cy="30" r="20" />
    <rect style="fill:#000000" x="60" y="20" width="30" height="20" />
  </g>
</svg>"#,
        png = base64(&ramp_png(200, 120)),
        image_transform = if image_transform.is_empty() {
            String::new()
        } else {
            format!("transform=\"{image_transform}\"")
        },
    )
}

/// Illustrator: unitless (points) 288 × 144, a gradient-painted height layer
/// whose gradient is defined in a *different* layer's defs, and a cutout that is
/// a bare top-level path with a hole drawn even-odd.
fn illustrator() -> String {
    r##"<?xml version="1.0" encoding="UTF-8"?>
<!-- Generator: Adobe Illustrator 28.0.0, SVG Export Plug-In . SVG Version: 1.2.0 Build 149)  -->
<svg xmlns="http://www.w3.org/2000/svg" version="1.1" viewBox="0 0 288 144">
  <defs>
    <style>.cls-1 { fill: url(#grad); }</style>
  </defs>
  <g id="Heights" data-name="Heights">
    <rect class="cls-1" x="0" y="0" width="288" height="144"/>
  </g>
  <g id="Spare_layer">
    <defs>
      <linearGradient id="grad" x1="0" y1="0" x2="288" y2="0" gradientUnits="userSpaceOnUse">
        <stop offset="0" stop-color="#000"/>
        <stop offset="1" stop-color="#fff"/>
      </linearGradient>
    </defs>
  </g>
  <path id="Cut_line" data-name="Cut line" fill-rule="evenodd" d="M 44 22 H 244 V 122 H 44 Z M 124 52 H 164 V 92 H 124 Z"/>
</svg>"##
        .into()
}

#[test]
fn sniffing_tells_svg_from_png() {
    assert!(sniff(inkscape("").as_bytes()));
    assert!(sniff(illustrator().as_bytes()));
    assert!(!sniff(&ramp_png(4, 4)));
}

#[test]
fn inkscape_layers_are_labelled_and_hidden_ones_kept() {
    let d = Drawing::parse(inkscape("").as_bytes()).unwrap();
    let labels: Vec<_> = d.layers().iter().map(|l| l.label.as_str()).collect();
    assert_eq!(labels, ["Photo", "Cut"]);
    assert!(d.layers()[1].hidden);
    assert_eq!((d.layers()[0].images, d.layers()[1].shapes), (1, 2));
    assert_eq!(d.guess_layers(), (0, Some(1)));
    let [w, h] = d.size_mm();
    assert!(
        (w - 100.0).abs() < 1e-3 && (h - 60.0).abs() < 1e-3,
        "{w} × {h}"
    );
}

#[test]
fn a_hidden_cutout_layer_still_cuts() {
    let d = Drawing::parse(inkscape("").as_bytes()).unwrap();
    let outline = d.outline(1).unwrap();
    // A stroked, unfilled circle still counts, and the rectangle is separate.
    assert_eq!(outline.pieces.len(), 2);
    let b = outline.bounds().unwrap();
    assert!(
        (b[0] - 10.0).abs() < 0.01 && (b[1] - 10.0).abs() < 0.01,
        "{b:?}"
    );
    assert!(
        (b[2] - 90.0).abs() < 0.01 && (b[3] - 50.0).abs() < 0.01,
        "{b:?}"
    );
    let circle = outline.pieces.iter().map(|p| p.area()).fold(0.0, f64::max);
    assert!(
        (circle - std::f64::consts::PI * 400.0).abs() < 2.0,
        "circle area {circle}"
    );
}

#[test]
fn an_embedded_16_bit_image_keeps_its_depth() {
    let d = Drawing::parse(inkscape("").as_bytes()).unwrap();
    let map = d.height_map(0, [0.0, 0.0, 100.0, 60.0]).unwrap();
    assert_eq!(map.bits, BitDepth::Sixteen);
    assert!(!map.was_colour);
    // A ramp, left to right, and more distinct levels than 8 bits could hold.
    let row: Vec<f32> = (0..map.width as usize)
        .map(|i| map.data[i + map.width as usize * 5])
        .collect();
    assert!(
        row[0] < 0.01 && row[row.len() - 1] > 0.99,
        "{} .. {}",
        row[0],
        row[row.len() - 1]
    );
    assert!(row.windows(2).all(|p| p[1] >= p[0] - 1e-6));
    // An 8-bit path would land every sample on a multiple of 1/255.
    let off_lattice = row
        .iter()
        .filter(|v| ((*v * 255.0) - (*v * 255.0).round()).abs() > 0.01)
        .count();
    assert!(
        off_lattice > row.len() / 2,
        "{off_lattice} of {} between 8-bit levels",
        row.len()
    );
    assert!(map.coverage.as_ref().unwrap().iter().all(|&c| c > 0.99));
}

#[test]
fn a_rotated_image_is_sampled_through_its_placement() {
    // Rotated 90° about the page centre and shrunk: the ramp now runs top to
    // bottom, and the page's left and right edges are uncovered.
    let d = Drawing::parse(inkscape("rotate(90 50 30) translate(20 12) scale(0.6)").as_bytes())
        .unwrap();
    let map = d.height_map(0, [0.0, 0.0, 100.0, 60.0]).unwrap();
    let (w, h) = (map.width as usize, map.height as usize);
    let cov = map.coverage.as_ref().unwrap();
    let mid = w / 2;
    assert!(cov[(h / 2) * w + mid] > 0.99, "the centre is covered");
    assert!(cov[(h / 2) * w + 1] < 0.01, "the far left is not");
    // Down the middle column the value climbs: the ramp turned with the image.
    let top = map.data[(h / 4) * w + mid];
    let bottom = map.data[(3 * h / 4) * w + mid];
    assert!(bottom > top + 0.2, "top {top}, bottom {bottom}");
}

#[test]
fn an_inkscape_drawing_becomes_closed_solids() {
    let d = Drawing::parse(inkscape("").as_bytes()).unwrap();
    let cutout = d.cutout(0, 1).unwrap();
    assert!((cutout.size_mm[0] - 80.0).abs() < 0.01 && (cutout.size_mm[1] - 40.0).abs() < 0.01);
    for tolerance in [0.0, 0.05] {
        let params = Params {
            size_px: 128,
            tolerance_mm: tolerance,
            ..Params::default()
        };
        let built = cutout.build(&params).unwrap();
        assert_eq!(built.pieces.len(), 2);
        for piece in &built.pieces {
            let report = check::inspect(&piece.solid());
            assert_eq!(report.watertight(), Some(true), "{report:?}");
        }
    }
}

#[test]
fn illustrator_points_layers_and_gradients() {
    let d = Drawing::parse(illustrator().as_bytes()).unwrap();
    let labels: Vec<_> = d.layers().iter().map(|l| l.label.as_str()).collect();
    assert_eq!(labels, ["Heights", "Spare layer", "Cut line"]);
    // 288 pt is four inches.
    let [w, h] = d.size_mm();
    assert!(
        (w - 101.6).abs() < 1e-3 && (h - 50.8).abs() < 1e-3,
        "{w} × {h}"
    );
    assert_eq!(d.guess_layers(), (0, Some(2)));

    // The gradient lives in another layer's defs and must still paint.
    let map = d.height_map(0, [0.0, 0.0, 101.6, 50.8]).unwrap();
    assert_eq!(map.bits, BitDepth::Eight);
    let w = map.width as usize;
    let row = &map.data[w * (map.height as usize / 2)..][..w];
    assert!(
        row[2] < 0.05 && row[w - 3] > 0.95,
        "{} .. {}",
        row[2],
        row[w - 3]
    );

    // Even-odd: a frame with a square hole, one piece, one hole.
    let cutout = d.cutout(0, 2).unwrap();
    assert_eq!(cutout.outline.pieces.len(), 1);
    assert_eq!(cutout.outline.pieces[0].holes(), 1);
    let built = cutout
        .build(&Params {
            size_px: 128,
            ..Params::default()
        })
        .unwrap();
    let report = check::inspect(&built.pieces[0].solid());
    assert_eq!(report.watertight(), Some(true), "{report:?}");
    assert_eq!(report.euler, Some(0), "a torus");
    // 200 × 100 pt outline, in millimetres.
    assert!((built.size_mm[0] - 200.0 * 25.4 / 72.0).abs() < 0.01);
}

#[test]
fn a_layer_hidden_by_a_css_class_still_cuts() {
    // Illustrator's internal-CSS export hides a layer through its class.
    let text = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- Generator: Adobe Illustrator 28.0.0, SVG Export Plug-In -->
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
  <defs><style>.cls-1 { fill: #808080; } .cls-2 { display: none; }</style></defs>
  <g id="Heights"><rect class="cls-1" width="100" height="100"/></g>
  <g id="Cut" class="cls-2"><circle cx="50" cy="50" r="40"/></g>
</svg>"#;
    let d = Drawing::parse(text.as_bytes()).unwrap();
    let outline = d.outline(1).unwrap();
    assert_eq!(outline.pieces.len(), 1);
}

#[test]
fn non_ascii_text_near_the_start_is_fine() {
    // A multi-byte character straddling the 4 kB header window.
    let label = "é".repeat(2100);
    let text = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:inkscape="http://www.inkscape.org/namespaces/inkscape" viewBox="0 0 10 10"><g inkscape:label="{label}"><rect width="10" height="10"/></g></svg>"#
    );
    let d = Drawing::parse(text.as_bytes()).unwrap();
    assert_eq!(d.layers()[0].label, label);
}

#[test]
fn a_layer_with_no_area_says_so() {
    let d = Drawing::parse(illustrator().as_bytes()).unwrap();
    // The spare layer holds nothing but a gradient definition.
    assert!(d.outline(1).is_err());
}

#[test]
fn plain_mode_is_the_image_itself() {
    let d = Drawing::parse(inkscape("").as_bytes()).unwrap();
    let map = d.plain(0).unwrap();
    assert!(map.coverage.is_none());
    assert_eq!(map.bits, BitDepth::Sixteen);
    let aspect = map.width as f32 / map.height as f32;
    assert!((aspect - 100.0 / 60.0).abs() < 0.02, "{aspect}");
}
