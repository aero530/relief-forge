//! Reading a relief out of a layered drawing.
//!
//! An SVG from Inkscape or Illustrator arrives with **layers**: one holds the
//! height data — an embedded grayscale image, or vector shapes painted in
//! grays and gradients — and another holds the **cutout**, the outline the
//! finished solid is cut to. [`Drawing`] lists the layers; [`Drawing::cutout`]
//! reads one of each into a [`Cutout`].
//!
//! # Layers are found in the XML, not the render tree
//!
//! A layer is a top-level element of the document: Inkscape writes
//! `<g inkscape:groupmode="layer" inkscape:label="…">`, Illustrator writes
//! `<g id="Layer_1" data-name="Layer 1">` — or, for a layer holding a single
//! object, sometimes the bare object. The parsed render tree keeps none of
//! that, and drops hidden layers outright, and a hidden cutout layer is a
//! perfectly normal way to work. So each layer is read on its own: the
//! document is rebuilt with only that layer visible, and with every other
//! top-level element moved into `<defs>`, so gradients, clip paths and styles
//! defined anywhere still resolve.
//!
//! # Heights keep their bit depth where they can
//!
//! Rasterising through resvg yields 8 bits per channel, which would quietly
//! crush a 16-bit PNG to 256 levels. So a height layer holding only embedded
//! images is sampled directly from the decoded images, through each image's
//! own placement — scale, rotation, skew — at full depth. Anything with vector
//! content is rendered by resvg and is honestly 8-bit, which
//! [`crate::params::Params::presmooth`] exists to deal with.
//!
//! # Units
//!
//! A drawing's coordinates become millimetres through its declared size. A
//! unitless or `px` size is CSS pixels — 96 to the inch — except in files
//! Illustrator wrote, where a unitless number is a point, 72 to the inch.
//! [`Drawing::size_mm`] shows the result; [`crate::params::Params::size_mm`]
//! overrides it when the guess is wrong.

use std::ops::Range;
use std::sync::Arc;

use resvg::tiny_skia;
use resvg::usvg::{self, roxmltree};

use crate::cutout::Cutout;
use crate::depth::{BitDepth, DepthMap};
use crate::outline::{FillRule, Outline, Region};
use crate::{Error, Result};

/// Longest side a height layer is rasterised to.
#[cfg(not(target_arch = "wasm32"))]
const MAX_RASTER: u32 = 4096;
#[cfg(target_arch = "wasm32")]
const MAX_RASTER: u32 = 2048;
/// Longest side for vector height layers, which have no native resolution.
const VECTOR_RASTER: u32 = 2048;
/// How closely curves are followed when flattened, in millimetres.
const FLATNESS_MM: f64 = 0.01;

const INKSCAPE_NS: &str = "http://www.inkscape.org/namespaces/inkscape";
const SODIPODI_NS: &str = "http://sodipodi.sourceforge.net/DTD/sodipodi-0.dtd";

/// One layer of a drawing.
#[derive(Debug, Clone)]
pub struct Layer {
    /// What the drawing program calls it.
    pub label: String,
    /// Hidden in the drawing program. Still usable here.
    pub hidden: bool,
    /// Embedded raster images inside it.
    pub images: usize,
    /// Vector shapes inside it.
    pub shapes: usize,
    /// The element's byte range in the source, and where its start tag ends.
    range: Range<usize>,
    start_tag_end: usize,
}

impl Layer {
    /// A short description for a picker: "photo — 1 image".
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.images > 0 {
            parts.push(plural(self.images, "image"));
        }
        if self.shapes > 0 {
            parts.push(plural(self.shapes, "shape"));
        }
        if parts.is_empty() {
            parts.push("empty".into());
        }
        let hidden = if self.hidden { ", hidden" } else { "" };
        format!("{} — {}{hidden}", self.label, parts.join(", "))
    }
}

fn plural(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// A parsed drawing: its source and its layers.
#[derive(Debug, Clone)]
pub struct Drawing {
    source: Arc<str>,
    layers: Vec<Layer>,
    /// The root element's start tag.
    root_start: Range<usize>,
    /// Where the root element's closing tag begins.
    root_end: usize,
    /// Pixels per inch for unitless lengths.
    dpi: f32,
    /// Document size in millimetres.
    size_mm: [f64; 2],
}

/// Does this look like an SVG rather than a raster image?
pub fn sniff(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    text.starts_with('<') && (text.contains("<svg") || text.starts_with("<?xml"))
}

impl Drawing {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::Svg("not UTF-8 text".into()))?
            .trim_start_matches('\u{feff}');
        let source: Arc<str> = Arc::from(text);
        let doc = roxmltree::Document::parse_with_options(
            &source,
            roxmltree::ParsingOptions {
                allow_dtd: true,
                ..Default::default()
            },
        )
        .map_err(|e| Error::Svg(e.to_string()))?;
        let root = doc.root_element();
        if root.tag_name().name() != "svg" {
            return Err(Error::Svg("the root element is not <svg>".into()));
        }

        // Illustrator writes unitless sizes in points.
        // By bytes: a slice of the text could split a multi-byte character.
        let head = &source.as_bytes()[..source.len().min(4096)];
        let illustrator = head.windows(17).any(|w| w == b"Adobe Illustrator");
        let dpi = if illustrator { 72.0 } else { 96.0 };

        let mut layers = Vec::new();
        for (k, node) in root.children().filter(|n| n.is_element()).enumerate() {
            let tag = node.tag_name();
            let name = tag.name();
            if matches!(
                name,
                "defs" | "metadata" | "title" | "desc" | "style" | "script" | "namedview"
            ) || tag.namespace() == Some(SODIPODI_NS)
            {
                continue;
            }
            let mut images = 0;
            let mut shapes = 0;
            for d in node.descendants().filter(|d| d.is_element()) {
                match d.tag_name().name() {
                    "image" => images += 1,
                    "path" | "rect" | "circle" | "ellipse" | "polygon" | "polyline" | "line"
                    | "use" => shapes += 1,
                    _ => {}
                }
            }
            let label = node
                .attribute((INKSCAPE_NS, "label"))
                .or_else(|| node.attribute("data-name"))
                .map(str::to_owned)
                .or_else(|| node.attribute("id").map(|id| id.replace('_', " ")))
                .unwrap_or_else(|| format!("{name} {}", k + 1));
            let style = node.attribute("style").unwrap_or("");
            let hidden = node.attribute("display") == Some("none")
                || style.replace(' ', "").contains("display:none");
            let range = node.range();
            layers.push(Layer {
                label,
                hidden,
                images,
                shapes,
                start_tag_end: start_tag_end(&source, range.start),
                range,
            });
        }
        if layers.is_empty() {
            return Err(Error::Svg("the drawing has nothing in it".into()));
        }

        let root_range = root.range();
        let root_start = root_range.start..start_tag_end(&source, root_range.start);
        let root_end = source[..root_range.end]
            .rfind("</")
            .unwrap_or(root_range.end);
        drop(doc);

        let mut this = Self {
            source,
            layers,
            root_start,
            root_end,
            dpi,
            size_mm: [0.0, 0.0],
        };
        // The document's own size, through the same parse that everything
        // else uses, so it agrees with the coordinates the layers produce.
        let tree = this.tree(None)?;
        let px = tree.size();
        this.size_mm = [this.mm(px.width() as f64), this.mm(px.height() as f64)];
        Ok(this)
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// The page size the document declares, in millimetres.
    pub fn size_mm(&self) -> [f64; 2] {
        self.size_mm
    }

    /// Drawing units (CSS pixels at this file's resolution) to millimetres.
    fn mm(&self, px: f64) -> f64 {
        px * 25.4 / self.dpi as f64
    }

    /// A first guess at which layer is which: the first with an embedded image
    /// holds the heights, and the topmost other layer with shapes and no
    /// images is the cutout.
    pub fn guess_layers(&self) -> (usize, Option<usize>) {
        let height = self.layers.iter().position(|l| l.images > 0).unwrap_or(0);
        let cutout = self
            .layers
            .iter()
            .enumerate()
            .rev()
            .find(|(k, l)| *k != height && l.shapes > 0 && l.images == 0)
            .map(|(k, _)| k);
        (height, cutout)
    }

    /// Build the document with only layer `only` drawn, or the whole document
    /// as it stands when `None`.
    fn tree(&self, only: Option<usize>) -> Result<usvg::Tree> {
        let text = match only {
            None => self.source.to_string(),
            Some(k) => self.isolate(k),
        };
        let options = usvg::Options {
            dpi: self.dpi,
            ..Default::default()
        };
        usvg::Tree::from_str(&text, &options).map_err(|e| Error::Svg(e.to_string()))
    }

    /// The document with layer `k` forced visible and every other top-level
    /// element moved into `<defs>`, where it is defined but not drawn.
    fn isolate(&self, k: usize) -> String {
        let src = &self.source;
        let target = &self.layers[k];
        let mut out = String::with_capacity(src.len() + 64);
        // Everything up to and including the root's start tag: the prolog
        // keeps any entities the document declares.
        out.push_str(&src[..self.root_start.end]);
        out.push_str("<defs>");
        // Every other child of the root — layers, defs, styles — verbatim.
        let body = self.root_start.end..self.root_end;
        let mut cursor = body.start;
        let mut others = String::new();
        for (j, layer) in self.layers.iter().enumerate() {
            if j == k {
                others.push_str(&src[cursor..layer.range.start]);
                cursor = layer.range.end;
            }
        }
        others.push_str(&src[cursor..body.end]);
        out.push_str(&others);
        out.push_str("</defs>");
        // The layer itself, with its own hiding switched off.
        let tag = &src[target.range.start..target.start_tag_end];
        out.push_str(&unhide(tag));
        out.push_str(&src[target.start_tag_end..target.range.end]);
        out.push_str("</svg>");
        out
    }

    /// The cutout layer's outline, resolved, in drawing millimetres (y down).
    pub fn outline(&self, layer: usize) -> Result<Outline> {
        let tree = self.tree(Some(layer))?;
        let mut regions = Vec::new();
        let flatness = FLATNESS_MM * self.dpi as f64 / 25.4;
        collect_regions(tree.root(), flatness, &mut regions);
        let to_mm = 25.4 / self.dpi as f64;
        let outline = Outline::resolve(&regions).map(|p| [p[0] * to_mm, p[1] * to_mm]);
        if outline.is_empty() {
            return Err(Error::Outline(format!(
                "the layer \"{}\" has no closed shapes to cut with",
                self.layers[layer].label
            )));
        }
        Ok(outline)
    }

    /// Height data from `layer` over `region` (drawing millimetres, y down,
    /// `[min_x, min_y, max_x, max_y]`), as a depth map whose first and last
    /// samples sit exactly on the region's edges.
    pub fn height_map(&self, layer: usize, region: [f64; 4]) -> Result<DepthMap> {
        let tree = self.tree(Some(layer))?;
        let to_px = self.dpi as f64 / 25.4;
        let region_px = region.map(|v| v * to_px);
        let (rw, rh) = (region_px[2] - region_px[0], region_px[3] - region_px[1]);
        if !(rw > 0.0 && rh > 0.0) {
            return Err(Error::Svg("the height layer has no area".into()));
        }

        let mut images = Vec::new();
        let vector = collect_images(tree.root(), &mut images);
        let long = if !vector && !images.is_empty() {
            // As fine as the finest image, measured in drawing pixels.
            let density = images.iter().map(|i| i.density()).fold(0.0f64, f64::max);
            (rw.max(rh) * density).ceil() as u32
        } else {
            VECTOR_RASTER
        }
        .clamp(16, MAX_RASTER);
        let (w, h) = if rw >= rh {
            (long, ((long as f64 * rh / rw).round() as u32).max(2))
        } else {
            (((long as f64 * rw / rh).round() as u32).max(2), long)
        };
        // Output sample (i, j) → drawing pixels.
        let at = |i: u32, j: u32| {
            [
                region_px[0] + i as f64 * rw / (w - 1) as f64,
                region_px[1] + j as f64 * rh / (h - 1) as f64,
            ]
        };

        if !vector && !images.is_empty() {
            Ok(sample_images(&images, w, h, at))
        } else {
            render_vector(&tree, w, h, region_px, rw, rh)
        }
    }

    /// The finished cutout: the outline of `cutout`, filled with the heights
    /// of `height`, over the outline's bounding box.
    pub fn cutout(&self, height: usize, cutout: usize) -> Result<Cutout> {
        let outline = self.outline(cutout)?;
        let b = outline.bounds().expect("checked non-empty");
        let map = self.height_map(height, b)?;
        Ok(Cutout {
            map,
            outline: outline.map(|p| [p[0] - b[0], p[1] - b[1]]),
            size_mm: [b[2] - b[0], b[3] - b[1]],
        })
    }

    /// The height layer alone, over its own extent, for a plain rectangular
    /// relief with no cutout.
    pub fn plain(&self, height: usize) -> Result<DepthMap> {
        let tree = self.tree(Some(height))?;
        let b = tree
            .root()
            .abs_stroke_bounding_box()
            .ok_or_else(|| Error::Svg("the height layer is empty".into()))?;
        let to_mm = 25.4 / self.dpi as f64;
        let region = [
            b.left() as f64 * to_mm,
            b.top() as f64 * to_mm,
            b.right() as f64 * to_mm,
            b.bottom() as f64 * to_mm,
        ];
        let mut map = self.height_map(height, region)?;
        // A rectangle with no cutout is the image itself: nothing to fill.
        if map
            .coverage
            .as_ref()
            .is_some_and(|c| c.iter().all(|&v| v > 0.99))
        {
            map.coverage = None;
        }
        Ok(map)
    }
}

/// Byte offset just past the `>` closing the start tag that begins at `start`.
fn start_tag_end(src: &str, start: usize) -> usize {
    let mut quote: Option<u8> = None;
    for (k, &b) in src.as_bytes()[start..].iter().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return start + k + 1,
            None => {}
        }
    }
    src.len()
}

/// Switch off `display:none` and `visibility:hidden` in a start tag.
///
/// Inline style outranks a stylesheet, so leading the `style` attribute with
/// `display:inline` also beats a class that hides the layer — which is how
/// Illustrator's internal-CSS export hides one.
fn unhide(tag: &str) -> String {
    let tag = tag
        .replace("display:none", "display:inline")
        .replace("display: none", "display:inline")
        .replace("display=\"none\"", "display=\"inline\"")
        .replace("display='none'", "display='inline'")
        .replace("visibility:hidden", "visibility:visible")
        .replace("visibility=\"hidden\"", "visibility=\"visible\"");
    for quote in ["style=\"", "style='"] {
        if let Some(at) = tag.find(quote) {
            let at = at + quote.len();
            return format!(
                "{}display:inline;visibility:visible;{}",
                &tag[..at],
                &tag[at..]
            );
        }
    }
    // No style attribute: add one straight after the element name.
    let name_end = tag
        .char_indices()
        .skip(1)
        .find(|(_, c)| c.is_whitespace() || *c == '/' || *c == '>')
        .map_or(tag.len(), |(k, _)| k);
    format!(
        "{} style=\"display:inline;visibility:visible\"{}",
        &tag[..name_end],
        &tag[name_end..]
    )
}

/// Every path in the tree as a filled region in drawing pixels.
///
/// Paint is ignored: a cutout is often drawn as a bare stroke with no fill, and
/// it is the geometry that matters. The fill rule is honoured where there is a
/// fill, and is SVG's default, non-zero, where there is not.
fn collect_regions(group: &usvg::Group, flatness: f64, out: &mut Vec<Region>) {
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => collect_regions(g, flatness, out),
            usvg::Node::Path(p) => {
                let rule = match p.fill().map(|f| f.rule()) {
                    Some(usvg::FillRule::EvenOdd) => FillRule::EvenOdd,
                    _ => FillRule::NonZero,
                };
                let loops = flatten(p.data(), p.abs_transform(), flatness);
                if !loops.is_empty() {
                    out.push(Region { loops, rule });
                }
            }
            usvg::Node::Image(_) | usvg::Node::Text(_) => {}
        }
    }
}

/// A path's subpaths as closed polylines, transformed, curves flattened to
/// within `flatness`.
fn flatten(path: &tiny_skia::Path, ts: tiny_skia::Transform, flatness: f64) -> Vec<Vec<[f64; 2]>> {
    use tiny_skia::PathSegment as S;
    let map = |p: tiny_skia::Point| {
        let (x, y) = (p.x as f64, p.y as f64);
        [
            ts.sx as f64 * x + ts.kx as f64 * y + ts.tx as f64,
            ts.ky as f64 * x + ts.sy as f64 * y + ts.ty as f64,
        ]
    };
    let mut loops = Vec::new();
    let mut current: Vec<[f64; 2]> = Vec::new();
    let close = |current: &mut Vec<[f64; 2]>, loops: &mut Vec<Vec<[f64; 2]>>| {
        if current.len() >= 3 {
            loops.push(std::mem::take(current));
        } else {
            current.clear();
        }
    };
    for segment in path.segments() {
        match segment {
            S::MoveTo(p) => {
                close(&mut current, &mut loops);
                current.push(map(p));
            }
            S::LineTo(p) => current.push(map(p)),
            S::QuadTo(c, p) => {
                let a = *current.last().unwrap_or(&map(c));
                let (c, p) = (map(c), map(p));
                let n = segments_for(&[a, c, p], flatness);
                for k in 1..=n {
                    let t = k as f64 / n as f64;
                    let u = 1.0 - t;
                    current.push([
                        u * u * a[0] + 2.0 * u * t * c[0] + t * t * p[0],
                        u * u * a[1] + 2.0 * u * t * c[1] + t * t * p[1],
                    ]);
                }
            }
            S::CubicTo(c1, c2, p) => {
                let a = *current.last().unwrap_or(&map(c1));
                let (c1, c2, p) = (map(c1), map(c2), map(p));
                let n = segments_for(&[a, c1, c2, p], flatness);
                for k in 1..=n {
                    let t = k as f64 / n as f64;
                    let u = 1.0 - t;
                    let (b0, b1, b2, b3) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    current.push([
                        b0 * a[0] + b1 * c1[0] + b2 * c2[0] + b3 * p[0],
                        b0 * a[1] + b1 * c1[1] + b2 * c2[1] + b3 * p[1],
                    ]);
                }
            }
            // An open subpath is closed anyway: a cutout has to enclose
            // something, and a drawing program fills an open path the same way.
            S::Close => close(&mut current, &mut loops),
        }
    }
    close(&mut current, &mut loops);
    loops
}

/// Segments needed for a Bézier's control polygon to stay within `flatness`
/// of the curve: the classic bound from the control points' distance to the
/// chord.
fn segments_for(points: &[[f64; 2]], flatness: f64) -> usize {
    let (a, b) = (points[0], points[points.len() - 1]);
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt();
    let deviation = points[1..points.len() - 1]
        .iter()
        .map(|p| {
            if len < 1e-12 {
                ((p[0] - a[0]).powi(2) + (p[1] - a[1]).powi(2)).sqrt()
            } else {
                ((p[0] - a[0]) * dy - (p[1] - a[1]) * dx).abs() / len
            }
        })
        .fold(0.0f64, f64::max);
    ((deviation / flatness).sqrt().ceil() as usize).clamp(1, 1024)
}

/// A decoded embedded image and where it sits.
struct Placed {
    luma: Vec<u16>,
    alpha: Vec<u16>,
    w: u32,
    h: u32,
    bits: BitDepth,
    colour: bool,
    /// Drawing pixels → image pixels.
    inverse: tiny_skia::Transform,
    /// The image's own size as the tree knows it, against which `inverse`
    /// is expressed.
    size: [f64; 2],
}

impl Placed {
    /// Image pixels per drawing pixel, along the image's finer axis.
    fn density(&self) -> f64 {
        let i = &self.inverse;
        let sx = (i.sx as f64).hypot(i.ky as f64);
        let sy = (i.kx as f64).hypot(i.sy as f64);
        sx.max(sy) * (self.w as f64 / self.size[0]).max(self.h as f64 / self.size[1])
    }
}

/// Gather the decodable raster images in a tree; returns whether anything
/// else — vector shapes, nested SVG — is drawn there too.
fn collect_images(group: &usvg::Group, out: &mut Vec<Placed>) -> bool {
    let mut vector = false;
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => vector |= collect_images(g, out),
            usvg::Node::Path(p) => vector |= p.is_visible(),
            usvg::Node::Text(_) => vector = true,
            usvg::Node::Image(img) => {
                let bytes = match img.kind() {
                    usvg::ImageKind::PNG(b) | usvg::ImageKind::JPEG(b) => b.clone(),
                    _ => {
                        vector = true;
                        continue;
                    }
                };
                let Ok(decoded) = image::load_from_memory(&bytes) else {
                    vector = true;
                    continue;
                };
                let Some(inverse) = img.abs_transform().invert() else {
                    continue;
                };
                use image::DynamicImage as D;
                let bits = match decoded {
                    D::ImageLuma16(_)
                    | D::ImageLumaA16(_)
                    | D::ImageRgb16(_)
                    | D::ImageRgba16(_) => BitDepth::Sixteen,
                    _ => BitDepth::Eight,
                };
                let colour = !matches!(
                    decoded,
                    D::ImageLuma8(_) | D::ImageLumaA8(_) | D::ImageLuma16(_) | D::ImageLumaA16(_)
                );
                let la = decoded.to_luma_alpha16();
                let (w, h) = la.dimensions();
                let raw = la.into_raw();
                let size = img.size();
                out.push(Placed {
                    luma: raw.iter().step_by(2).copied().collect(),
                    alpha: raw.iter().skip(1).step_by(2).copied().collect(),
                    w,
                    h,
                    bits,
                    colour,
                    inverse,
                    size: [size.width() as f64, size.height() as f64],
                });
            }
        }
    }
    vector
}

/// Sample images directly, at their own bit depth, painting later ones over
/// earlier ones as the drawing does.
fn sample_images(images: &[Placed], w: u32, h: u32, at: impl Fn(u32, u32) -> [f64; 2]) -> DepthMap {
    let n = (w * h) as usize;
    let mut data = vec![0.0f32; n];
    let mut coverage = vec![0.0f32; n];
    let scale = 1.0 / u16::MAX as f32;
    for img in images {
        let t = &img.inverse;
        let (fx, fy) = (img.w as f64 / img.size[0], img.h as f64 / img.size[1]);
        let pixel = |x: i64, y: i64| {
            let x = x.clamp(0, img.w as i64 - 1) as usize;
            let y = y.clamp(0, img.h as i64 - 1) as usize;
            let k = y * img.w as usize + x;
            (img.luma[k] as f32 * scale, img.alpha[k] as f32 * scale)
        };
        for j in 0..h {
            for i in 0..w {
                let [x, y] = at(i, j);
                // Into the image's own size, then into its pixels; pixel k
                // covers [k, k+1), so its centre is at k + 0.5.
                let u = (t.sx as f64 * x + t.kx as f64 * y + t.tx as f64) * fx;
                let v = (t.ky as f64 * x + t.sy as f64 * y + t.ty as f64) * fy;
                // The region's edges are often the image's own edges, which
                // the transforms' f32 arithmetic lands a hair either side of.
                const SLACK: f64 = 1e-3;
                if u < -SLACK || v < -SLACK || u > img.w as f64 + SLACK || v > img.h as f64 + SLACK
                {
                    continue;
                }
                let (su, sv) = (u - 0.5, v - 0.5);
                let (x0, y0) = (su.floor() as i64, sv.floor() as i64);
                let (tx, ty) = ((su - x0 as f64) as f32, (sv - y0 as f64) as f32);
                let mix = |a: (f32, f32), b: (f32, f32), t: f32| {
                    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
                };
                let top = mix(pixel(x0, y0), pixel(x0 + 1, y0), tx);
                let bottom = mix(pixel(x0, y0 + 1), pixel(x0 + 1, y0 + 1), tx);
                let (value, alpha) = mix(top, bottom, ty);
                let k = (j * w + i) as usize;
                data[k] = data[k] * (1.0 - alpha) + value * alpha;
                coverage[k] = coverage[k] + alpha * (1.0 - coverage[k]);
            }
        }
    }
    DepthMap {
        width: w,
        height: h,
        bits: if images.iter().all(|i| i.bits == BitDepth::Sixteen) {
            BitDepth::Sixteen
        } else {
            BitDepth::Eight
        },
        was_colour: images.iter().any(|i| i.colour),
        data,
        coverage: Some(coverage),
    }
}

/// Render a vector layer with resvg: luma from the colour, coverage from alpha.
fn render_vector(
    tree: &usvg::Tree,
    w: u32,
    h: u32,
    region_px: [f64; 4],
    rw: f64,
    rh: f64,
) -> Result<DepthMap> {
    let mut pixmap =
        tiny_skia::Pixmap::new(w, h).ok_or_else(|| Error::Svg("raster too large".into()))?;
    // Sample i sits on region x0 + i·rw/(w-1); resvg samples pixel centres,
    // i + 0.5, hence the half-pixel shift.
    let (sx, sy) = ((w - 1) as f64 / rw, (h - 1) as f64 / rh);
    let ts = tiny_skia::Transform::from_row(
        sx as f32,
        0.0,
        0.0,
        sy as f32,
        (0.5 - region_px[0] * sx) as f32,
        (0.5 - region_px[1] * sy) as f32,
    );
    resvg::render(tree, ts, &mut pixmap.as_mut());

    let n = (w * h) as usize;
    let mut data = Vec::with_capacity(n);
    let mut coverage = Vec::with_capacity(n);
    let mut colour = false;
    for px in pixmap.pixels() {
        let a = px.alpha();
        if a == 0 {
            data.push(0.0);
            coverage.push(0.0);
            continue;
        }
        let c = px.demultiply();
        let (r, g, b) = (c.red() as f32, c.green() as f32, c.blue() as f32);
        colour |= (r - g).abs() > 2.0 || (g - b).abs() > 2.0;
        data.push((0.299 * r + 0.587 * g + 0.114 * b) / 255.0);
        coverage.push(a as f32 / 255.0);
    }
    Ok(DepthMap {
        width: w,
        height: h,
        bits: BitDepth::Eight,
        was_colour: colour,
        data,
        coverage: Some(coverage),
    })
}

#[cfg(test)]
mod tests;
