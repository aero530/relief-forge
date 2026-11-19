//! `relief` — the same geometry as the GUI, without the GUI.
//!
//! Exists for three reasons: batch conversion, a way to check the mesh that is
//! too expensive to run on every rebuild in the app ([`relief_core::check`]),
//! and as proof that the core carries no dependency on Bevy or egui.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use relief_core::cutout::{CutRelief, Cutout};
use relief_core::depth::{Albedo, BitDepth, DepthMap, check_aspect};
use relief_core::export::{self, Format};
use relief_core::params::{Orientation, Params, Units};
use relief_core::svg::{self, Drawing};
use relief_core::{Decimation, Solid, Stats, check, sample, stats};

#[derive(Parser)]
#[command(
    name = "relief",
    version,
    about = "Turn a grayscale depth map into a watertight bas-relief mesh."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Convert a depth map, or a layered SVG drawing, to a printable mesh.
    ///
    /// A drawing whose cutout has several separate pieces writes one file per
    /// piece, numbered: `name-1.stl`, `name-2.stl`, …
    Convert {
        /// 16-bit PNG, 8-bit PNG, JPEG, or SVG.
        depth: PathBuf,
        /// Where to write. Defaults to the input's name with the format's
        /// extension.
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Photo to colour the relief with (OBJ and PLY only).
        #[arg(long)]
        photo: Option<PathBuf>,
        #[arg(long, value_enum, default_value_t = FormatArg::Stl)]
        format: FormatArg,
        /// Walk the edges and report whether the result is watertight. Costs
        /// memory proportional to the triangle count, so it is off by default.
        #[arg(long)]
        check: bool,
        #[command(flatten)]
        layers: LayerArgs,
        #[command(flatten)]
        params: ParamArgs,
    },
    /// Report what a conversion would produce, without writing it.
    Info {
        depth: PathBuf,
        #[arg(long, value_enum, default_value_t = FormatArg::Stl)]
        format: FormatArg,
        #[command(flatten)]
        layers: LayerArgs,
        #[command(flatten)]
        params: ParamArgs,
    },
    /// List an SVG drawing's layers, and which would be used for what.
    Layers { drawing: PathBuf },
    /// Write a synthetic depth map, for trying the app out.
    Sample {
        #[arg(short, long, default_value = "sample_depth_16bit.png")]
        out: PathBuf,
        #[arg(long, default_value_t = 1536)]
        width: u32,
        #[arg(long, default_value_t = 1152)]
        height: u32,
        /// 8 to exercise the JPEG-grade path, 16 for a real depth map.
        #[arg(long, default_value_t = 16)]
        bits: u8,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum FormatArg {
    Stl,
    Obj,
    Ply,
}

impl From<FormatArg> for Format {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Stl => Format::Stl,
            FormatArg::Obj => Format::Obj,
            FormatArg::Ply => Format::Ply,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum UnitsArg {
    Mm,
    In,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum OrientationArg {
    FaceUp,
    Upright,
}

/// Which layers of an SVG drawing to use. Ignored for raster inputs.
#[derive(Args, Clone)]
struct LayerArgs {
    /// Layer holding the heights — an embedded image, or gray shapes and
    /// gradients. A name, or a number from `relief layers`. Defaults to the
    /// first layer with an image in it.
    #[arg(long)]
    height_layer: Option<String>,
    /// Layer whose shapes cut the outline, or `none` for a plain rectangle.
    /// Defaults to the topmost other layer with shapes and no images.
    #[arg(long)]
    cutout_layer: Option<String>,
}

/// Every geometry knob, as an override on top of the defaults or a settings
/// file. `None` means "leave whatever was there", so `--settings` and individual
/// flags compose the way a person expects.
#[derive(Args, Clone)]
struct ParamArgs {
    /// Settings JSON, as saved by the GUI.
    #[arg(long)]
    settings: Option<PathBuf>,
    /// Near plane, 0..1.
    #[arg(long)]
    near: Option<f32>,
    /// Far plane, 0..1.
    #[arg(long)]
    far: Option<f32>,
    /// Emboss height in mm — how far the relief stands out of its plate.
    #[arg(long)]
    emboss: Option<f32>,
    /// Flip the depth map (for maps where near is dark).
    #[arg(long)]
    invert: bool,
    /// Longest side of the mesh grid, in pixels.
    #[arg(long)]
    px: Option<u32>,
    /// Longest outer side of the printed plaque, in millimetres. For a cutout
    /// drawing, the outline's longest side — instead of the drawing's own size.
    #[arg(long)]
    mm: Option<f32>,
    /// Median window: 1, 3 or 5.
    #[arg(long)]
    smooth: Option<u32>,
    /// Gaussian pre-smoothing for 8-bit inputs, in pixels.
    #[arg(long)]
    presmooth: Option<f32>,
    /// Frame thickness in mm. Zero means no frame at all.
    #[arg(long)]
    thickness: Option<f32>,
    /// Height of the frame face above the relief, in mm. Negative sinks it in.
    #[arg(long)]
    frame_near: Option<f32>,
    /// Back plate thickness in mm.
    #[arg(long)]
    frame_back: Option<f32>,
    /// Unit the exported file's numbers are written in.
    #[arg(long, value_enum)]
    units: Option<UnitsArg>,
    /// Largest deviation the simplified surface may have, in mm. 0 keeps every
    /// grid vertex.
    #[arg(long)]
    tolerance: Option<f32>,
    /// Printer layer height in mm, for the step-size check.
    #[arg(long)]
    layer: Option<f32>,
    #[arg(long, value_enum)]
    orientation: Option<OrientationArg>,
}

impl ParamArgs {
    fn resolve(&self) -> Result<Params, String> {
        let mut p = match &self.settings {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("reading {}: {e}", path.display()))?;
                serde_json::from_str(&text)
                    .map_err(|e| format!("{} is not settings JSON: {e}", path.display()))?
            }
            None => Params::default(),
        };

        if let Some(v) = self.near {
            p.near = v;
        }
        if let Some(v) = self.far {
            p.far = v;
        }
        if let Some(v) = self.emboss {
            p.emboss_mm = v;
        }
        if self.invert {
            p.invert = true;
        }
        if let Some(v) = self.px {
            p.size_px = v;
        }
        if let Some(v) = self.mm {
            p.size_mm = v;
            p.use_drawing_size = false;
        }
        if let Some(v) = self.smooth {
            p.filter_size = v;
        }
        if let Some(v) = self.presmooth {
            p.presmooth = v;
        }
        if let Some(v) = self.thickness {
            p.frame_thickness_mm = v;
        }
        if let Some(v) = self.frame_near {
            p.frame_near_mm = v;
        }
        if let Some(v) = self.frame_back {
            p.frame_back_mm = v;
        }
        if let Some(v) = self.units {
            p.units = match v {
                UnitsArg::Mm => Units::Mm,
                UnitsArg::In => Units::In,
            };
        }
        if let Some(v) = self.tolerance {
            p.tolerance_mm = v;
        }
        if let Some(v) = self.layer {
            p.layer_mm = v;
        }
        if let Some(v) = self.orientation {
            p.orientation = match v {
                OrientationArg::FaceUp => Orientation::FaceUp,
                OrientationArg::Upright => Orientation::Upright,
            };
        }

        p.clamp();
        p.validate().map_err(|e| e.to_string())?;
        Ok(p)
    }
}

fn main() {
    if let Err(message) = run() {
        eprintln!("relief: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    match Cli::parse().command {
        Command::Convert {
            depth,
            out,
            photo,
            format,
            check: run_check,
            layers,
            params,
        } => convert(
            &depth,
            out,
            photo,
            format.into(),
            run_check,
            &layers,
            &params,
        ),
        Command::Info {
            depth,
            format,
            layers,
            params,
        } => info(&depth, format.into(), &layers, &params),
        Command::Layers { drawing } => list_layers(&drawing),
        Command::Sample {
            out,
            width,
            height,
            bits,
        } => write_sample(&out, width, height, bits),
    }
}

/// What an input file turned out to be.
enum Source {
    Plain(DepthMap),
    Cut(Cutout),
}

fn load(path: &Path, layers: &LayerArgs) -> Result<Source, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let at = |e: relief_core::Error| format!("{}: {e}", path.display());
    if !svg::sniff(&bytes) {
        return DepthMap::decode(&bytes).map(Source::Plain).map_err(at);
    }
    let drawing = Drawing::parse(&bytes).map_err(at)?;
    let (guess_height, guess_cutout) = drawing.guess_layers();
    let height = match &layers.height_layer {
        Some(name) => pick_layer(&drawing, name)?,
        None => guess_height,
    };
    let cutout = match layers.cutout_layer.as_deref() {
        Some(name) if name.eq_ignore_ascii_case("none") => None,
        Some(name) => Some(pick_layer(&drawing, name)?),
        None => guess_cutout,
    };
    let label = |k: usize| drawing.layers()[k].label.clone();
    match cutout {
        None => {
            eprintln!("  heights from \"{}\", no cutout", label(height));
            drawing.plain(height).map(Source::Plain).map_err(at)
        }
        Some(cut) => {
            eprintln!(
                "  heights from \"{}\", cut by \"{}\"",
                label(height),
                label(cut)
            );
            drawing.cutout(height, cut).map(Source::Cut).map_err(at)
        }
    }
}

/// A layer by 1-based number or by label, ignoring case.
fn pick_layer(drawing: &Drawing, name: &str) -> Result<usize, String> {
    let layers = drawing.layers();
    if let Ok(n) = name.parse::<usize>()
        && (1..=layers.len()).contains(&n)
    {
        return Ok(n - 1);
    }
    layers
        .iter()
        .position(|l| l.label.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let names: Vec<_> = layers.iter().map(|l| format!("\"{}\"", l.label)).collect();
            format!(
                "no layer called \"{name}\"; the drawing has {}",
                names.join(", ")
            )
        })
}

fn list_layers(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let drawing = Drawing::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    let [w, h] = drawing.size_mm();
    println!("{}: {w:.1} × {h:.1} mm", path.display());
    let (height, cutout) = drawing.guess_layers();
    for (k, layer) in drawing.layers().iter().enumerate() {
        let role = if k == height {
            "  ← heights"
        } else if Some(k) == cutout {
            "  ← cutout"
        } else {
            ""
        };
        println!("  {:>2}  {}{role}", k + 1, layer.summary());
    }
    Ok(())
}

/// Stats for a cutout: every piece's, totalled over the drawing.
fn cut_stats(relief: &CutRelief, format: Format, params: &Params) -> Stats {
    let parts: Vec<Stats> = relief
        .pieces
        .iter()
        .map(|p| Stats::new(&p.solid(), format, params))
        .collect();
    Stats::combine(
        &parts,
        (relief.field.w, relief.field.h),
        relief.extents_mm(params),
    )
    .expect("a built cutout has at least one piece")
}

/// `name.stl` for one piece; `name-1.stl`, `name-2.stl`, … for several.
fn piece_path(out: &Path, k: usize, count: usize) -> PathBuf {
    if count == 1 {
        return out.to_path_buf();
    }
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy())
        .unwrap_or_default();
    let name = match out.extension() {
        Some(ext) => format!("{stem}-{}.{}", k + 1, ext.to_string_lossy()),
        None => format!("{stem}-{}", k + 1),
    };
    out.with_file_name(name)
}

fn convert(
    depth_path: &Path,
    out: Option<PathBuf>,
    photo_path: Option<PathBuf>,
    format: Format,
    run_check: bool,
    layers: &LayerArgs,
    args: &ParamArgs,
) -> Result<(), String> {
    let params = args.resolve()?;
    let out = out.unwrap_or_else(|| depth_path.with_extension(format.extension()));
    let depth = match load(depth_path, layers)? {
        Source::Plain(depth) => depth,
        Source::Cut(cutout) => {
            if photo_path.is_some() {
                eprintln!("relief: warning: --photo is not used with a cutout drawing");
            }
            let relief = cutout
                .build(&params)
                .map_err(|e| format!("{}: {e}", depth_path.display()))?;
            report(&cut_stats(&relief, format, &params), &params);
            let count = relief.pieces.len();
            for (k, piece) in relief.pieces.iter().enumerate() {
                write_solid(
                    &piece.solid(),
                    format,
                    &piece_path(&out, k, count),
                    run_check,
                )?;
            }
            return Ok(());
        }
    };
    let field = depth
        .to_height_field(&params)
        .map_err(|e| format!("{}: {e}", depth_path.display()))?;

    let albedo = match &photo_path {
        None => None,
        Some(path) => {
            if !format.carries_colour() {
                eprintln!(
                    "relief: warning: {} cannot store colour, so --photo is ignored \
                     (use --format ply)",
                    format.label()
                );
                None
            } else {
                let bytes =
                    std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
                let decoded = image::load_from_memory(&bytes)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                check_aspect(&depth, decoded.width(), decoded.height())
                    .map_err(|e| e.to_string())?;
                Some(Albedo::from_dynamic(&decoded, field.w, field.h))
            }
        }
    };

    let decimation = Decimation::build(&field, params.tolerance_mm);
    let solid = Solid::new(&field, &params)
        .with_albedo(albedo.as_ref())
        .with_decimation(decimation.as_ref());
    let stats = Stats::new(&solid, format, &params);
    report(&stats, &params);
    write_solid(&solid, format, &out, run_check)
}

/// Stream one solid to a file, and optionally prove it closed.
fn write_solid(solid: &Solid, format: Format, out: &Path, run_check: bool) -> Result<(), String> {
    let mut file =
        std::fs::File::create(out).map_err(|e| format!("creating {}: {e}", out.display()))?;
    let mut last_percent = -1i32;
    export::write(format, solid, &mut file, &mut |t| {
        let percent = (t * 100.0) as i32;
        if percent > last_percent {
            last_percent = percent;
            eprint!("\r  writing {}… {percent:>3}%", out.display());
            let _ = std::io::stderr().flush();
        }
    })
    .map_err(|e| format!("writing {}: {e}", out.display()))?;
    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "\r  wrote {} ({})           ",
        out.display(),
        stats::human_bytes(bytes)
    );

    if run_check {
        eprint!("  checking the mesh… ");
        let report = check::inspect(solid);
        match report.watertight() {
            Some(true) => println!(
                "watertight: closed, genus {}, outward-wound",
                (2 - report.expected_euler) / 2
            ),
            _ => {
                println!(
                    "NOT watertight: {} open edge(s), {} non-manifold edge(s), euler {:?}",
                    report.boundary_edges.unwrap_or(0),
                    report.non_manifold_edges.unwrap_or(0),
                    report.euler
                );
                return Err("the mesh is not printable — this is a bug, please report it".into());
            }
        }
    }
    Ok(())
}

fn info(
    depth_path: &Path,
    format: Format,
    layers: &LayerArgs,
    args: &ParamArgs,
) -> Result<(), String> {
    let params = args.resolve()?;
    let depth = match load(depth_path, layers)? {
        Source::Plain(depth) => depth,
        Source::Cut(cutout) => {
            let relief = cutout
                .build(&params)
                .map_err(|e| format!("{}: {e}", depth_path.display()))?;
            let [w, h] = cutout.size_mm;
            println!(
                "{}: cutout {w:.1} × {h:.1} mm in the drawing, heights {}×{} {}",
                depth_path.display(),
                cutout.map.width,
                cutout.map.height,
                cutout.map.bits.label()
            );
            report(&cut_stats(&relief, format, &params), &params);
            return Ok(());
        }
    };
    let field = depth
        .to_height_field(&params)
        .map_err(|e| format!("{}: {e}", depth_path.display()))?;
    let decimation = Decimation::build(&field, params.tolerance_mm);
    let solid = Solid::new(&field, &params).with_decimation(decimation.as_ref());
    println!(
        "{}: {}×{} {}{}",
        depth_path.display(),
        depth.width,
        depth.height,
        depth.bits.label(),
        if depth.was_colour {
            " (colour, converted to luma — a depth map should be grayscale)"
        } else {
            ""
        }
    );
    report(&Stats::new(&solid, format, &params), &params);
    Ok(())
}

fn report(stats: &Stats, _params: &Params) {
    println!(
        "  grid       {}×{} at {}",
        stats.grid.0,
        stats.grid.1,
        stats.pitch_line()
    );
    println!(
        "  mesh       {} triangles, {} vertices{}",
        stats.triangles,
        stats.vertices,
        if stats.slicer_strain() {
            "  ** more than most slicers handle comfortably **"
        } else {
            ""
        }
    );
    println!("  simplify   {}", stats.simplify_line());
    println!(
        "  size       {}{}",
        stats.bbox_line(),
        if stats.pieces > 1 {
            format!("  ({} separate pieces, one file each)", stats.pieces)
        } else if stats.framed {
            String::new()
        } else {
            "  (no frame)".into()
        }
    );
    println!("  relief     {}", stats.relief_line());
    println!("  levels     {} ({})", stats.levels, stats.bits.label());
    println!(
        "  step       {}{}",
        stats.step_line(),
        if stats.steps_ok() { "" } else { "  **" }
    );
    println!(
        "  volume     {:.1} cm³ ≈ {:.0} g PLA",
        stats.volume_mm3 / 1000.0,
        stats.filament_g
    );
    println!("  file       {}", stats.file_line());
}

fn write_sample(out: &Path, width: u32, height: u32, bits: u8) -> Result<(), String> {
    let bits = match bits {
        8 => BitDepth::Eight,
        16 => BitDepth::Sixteen,
        other => return Err(format!("--bits must be 8 or 16, not {other}")),
    };
    let map = sample::depth_map_with(width, height, bits);
    let png = sample::to_png(&map).map_err(|e| e.to_string())?;
    std::fs::write(out, &png).map_err(|e| format!("writing {}: {e}", out.display()))?;
    println!(
        "wrote {} — {width}×{height} {} ({})",
        out.display(),
        bits.label(),
        stats::human_bytes(png.len() as u64)
    );
    Ok(())
}
