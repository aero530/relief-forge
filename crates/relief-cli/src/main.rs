//! `relief` — the same geometry as the GUI, without the GUI.
//!
//! Exists for three reasons: batch conversion, a way to check the mesh that is
//! too expensive to run on every rebuild in the app ([`relief_core::check`]),
//! and as proof that the core carries no dependency on Bevy or egui.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};
use relief_core::depth::{Albedo, BitDepth, DepthMap, check_aspect};
use relief_core::export::{self, Format};
use relief_core::params::{Orientation, Params, Units};
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
    /// Convert a depth map to a printable mesh.
    Convert {
        /// 16-bit PNG, 8-bit PNG, or JPEG.
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
        params: ParamArgs,
    },
    /// Report what a conversion would produce, without writing it.
    Info {
        depth: PathBuf,
        #[arg(long, value_enum, default_value_t = FormatArg::Stl)]
        format: FormatArg,
        #[command(flatten)]
        params: ParamArgs,
    },
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
    /// Longest outer side of the printed plaque, in millimetres.
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
            params,
        } => convert(&depth, out, photo, format.into(), run_check, &params),
        Command::Info {
            depth,
            format,
            params,
        } => info(&depth, format.into(), &params),
        Command::Sample {
            out,
            width,
            height,
            bits,
        } => write_sample(&out, width, height, bits),
    }
}

fn load_depth(path: &Path) -> Result<DepthMap, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    DepthMap::decode(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn convert(
    depth_path: &Path,
    out: Option<PathBuf>,
    photo_path: Option<PathBuf>,
    format: Format,
    run_check: bool,
    args: &ParamArgs,
) -> Result<(), String> {
    let params = args.resolve()?;
    let depth = load_depth(depth_path)?;
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
    let out = out.unwrap_or_else(|| depth_path.with_extension(format.extension()));

    let stats = Stats::new(&solid, format, &params);
    report(&stats, &params);

    let file =
        std::fs::File::create(&out).map_err(|e| format!("creating {}: {e}", out.display()))?;
    let mut file = file;
    let mut last_percent = -1i32;
    export::write(format, &solid, &mut file, &mut |t| {
        let percent = (t * 100.0) as i32;
        if percent > last_percent {
            last_percent = percent;
            eprint!("\r  writing {}… {percent:>3}%", out.display());
            let _ = std::io::stderr().flush();
        }
    })
    .map_err(|e| format!("writing {}: {e}", out.display()))?;
    eprintln!(
        "\r  wrote {} ({})           ",
        out.display(),
        stats.file_line()
    );

    if run_check {
        eprint!("  checking the mesh… ");
        let report = check::inspect(&solid);
        match report.watertight() {
            Some(true) => println!("watertight: closed, genus 0, outward-wound"),
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

fn info(depth_path: &Path, format: Format, args: &ParamArgs) -> Result<(), String> {
    let params = args.resolve()?;
    let depth = load_depth(depth_path)?;
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
        if stats.framed { "" } else { "  (no frame)" }
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
