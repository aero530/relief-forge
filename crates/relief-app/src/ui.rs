//! The egui shell: toolbar, settings, depth panes, viewport, status bar.
//!
//! Panel choice is not free-form: bottom and right panels neither reserve space
//! nor render inside the background-layer `Ui` that bevy_egui hands us, while
//! top and left ones behave normally. So the status line rides along under the
//! toolbar, and the two columns are both left panels.

use bevy::prelude::*;
use bevy_egui::{EguiContexts, egui};
use relief_core::depth::BitDepth;
use relief_core::export::Format;
use relief_core::params::{
    EMBOSS_MM_MAX, EMBOSS_MM_MIN, Orientation, Params, SIZE_MM_MAX, SIZE_MM_MIN, SIZE_PX_MIN,
    SIZE_PX_UPSTREAM_MAX, TOLERANCE_MM_MAX, Units,
};

use crate::platform::{self, PickKind, Saved};
use crate::rebuild;
use crate::state::{AppState, Status, Tone};
use crate::viewport::Viewport;

/// Width of the label column in the settings panel.
const LABEL_WIDTH: f32 = 104.0;

/// What the last completed build measured, in the form the panel shows it.
///
/// Read out of the stats before the parameters are borrowed mutably, which is
/// the only reason it exists as a struct rather than a handful of calls.
struct Measured {
    grid: (usize, usize),
    pitch: String,
    triangles: String,
    vertices: String,
    volume_mm3: f64,
    filament_g: f32,
    file_line: String,
    simplify: String,
    simplify_blocked: bool,
}

pub fn draw(
    mut contexts: EguiContexts,
    mut state: ResMut<AppState>,
    mut viewport: ResMut<Viewport>,
    windows: Query<&Window>,
    time: Res<Time>,
) -> Result {
    // The texture id must be resolved before `ctx_mut` borrows `contexts`.
    let texture = contexts.image_id(&viewport.image.clone());
    let ctx = contexts.ctx_mut()?.clone();
    let now = time.elapsed_secs_f64();
    let window_height = windows.iter().next().map(|w| w.height()).unwrap_or(0.0);

    // egui 0.36 hangs panels off a Ui covering the viewport rather than the
    // context directly; this is the shape every bevy_egui example uses.
    let mut root = egui::Ui::new(
        ctx.clone(),
        "viewport".into(),
        egui::UiBuilder::new()
            .layer_id(egui::LayerId::background())
            .max_rect(ctx.viewport_rect()),
    );

    egui::Panel::top("toolbar").show(&mut root, |ui| {
        toolbar(ui, &mut state, now);
        ui.separator();
        status_bar(ui, &state);
    });

    egui::Panel::left("settings")
        .default_size(292.0)
        .resizable(true)
        .show(&mut root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| settings(ui, &mut state, now));
        });

    egui::Panel::left("panes")
        .default_size(228.0)
        .resizable(true)
        .show(&mut root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| panes(ui, &mut state, window_height));
        });

    egui::CentralPanel::default().show(&mut root, |ui| {
        view_area(ui, &state, &mut viewport, texture);
    });

    Ok(())
}

// ---------------------------------------------------------------------------
// Toolbar and status
// ---------------------------------------------------------------------------

fn toolbar(ui: &mut egui::Ui, state: &mut AppState, now: f64) {
    ui.horizontal_wrapped(|ui| {
        if ui
            .button("Open depth…")
            .on_hover_text("16-bit PNG, 8-bit PNG, or JPEG")
            .clicked()
        {
            platform::open(&state.inbox, PickKind::Depth);
        }
        if ui
            .button("Open image…")
            .on_hover_text("A photo to colour the relief with. Must match the depth map's shape.")
            .clicked()
        {
            platform::open(&state.inbox, PickKind::Photo);
        }

        ui.separator();

        egui::ComboBox::from_id_salt("format")
            .selected_text(state.format.label())
            .width(64.0)
            .show_ui(ui, |ui| {
                for format in Format::ALL {
                    let label = match format {
                        Format::Stl => "STL — every slicer",
                        Format::Ply => "PLY — binary, keeps colour",
                        Format::Obj => "OBJ — text, keeps colour",
                    };
                    ui.selectable_value(&mut state.format, format, label);
                }
            });
        let export = ui.button("Export…").on_hover_text(match state.stats() {
            Some(stats) => format!(
                "Rebuilds at {} px and streams straight to the file — about {}",
                state.params.size_px,
                stats.file_line()
            ),
            None => "Rebuild at full resolution and write the mesh".into(),
        });
        if export.clicked() {
            rebuild::export_now(state);
        }

        ui.separator();

        if ui.button("Save settings").clicked() {
            let text = serde_json::to_string_pretty(&state.params).unwrap_or_default();
            state.status = match platform::save_stream("relief-settings.json", |sink| {
                std::io::Write::write_all(sink, text.as_bytes()).map_err(|e| e.to_string())
            }) {
                Err(message) => Status::error(message),
                Ok(Saved::Cancelled) => Status::info("Not saved"),
                Ok(Saved::Downloaded) => Status::info("Settings downloaded"),
                Ok(Saved::To(path)) => Status::info(format!("Settings written to {path}")),
            };
        }
        if ui.button("Load settings").clicked() {
            platform::open(&state.inbox, PickKind::Settings);
        }

        ui.separator();
        ui.label(egui::RichText::new("sample").weak());
        let sixteen = state.depth.name.ends_with("16bit.png");
        let eight = state.depth.name.ends_with("gray.jpg");
        if ui.selectable_label(sixteen, "16-bit").clicked() {
            rebuild::load_sample(state, false, now);
        }
        if ui
            .selectable_label(eight, "8-bit")
            .on_hover_text("The same terrain quantised to 256 levels with JPEG-style block noise")
            .clicked()
        {
            rebuild::load_sample(state, true, now);
        }
    });
}

fn status_bar(ui: &mut egui::Ui, state: &AppState) {
    let colour = match state.status.tone {
        Tone::Info => ui.visuals().text_color(),
        Tone::Warn => egui::Color32::from_rgb(0xE0, 0xA0, 0x40),
        Tone::Error => egui::Color32::from_rgb(0xE0, 0x70, 0x60),
    };
    // Its own row, and truncated: a restored-settings message carries a full
    // path, which would otherwise run straight through the readouts.
    ui.add(
        egui::Label::new(egui::RichText::new(&state.status.text).color(colour))
            .truncate()
            .selectable(true),
    )
    .on_hover_text(&state.status.text);

    ui.horizontal_wrapped(|ui| {
        let Some(stats) = state.stats() else {
            return;
        };
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(rebuild::preview_note(state)).weak());
            ui.separator();
            ui.label(format!("{} triangles", thousands(stats.triangles)));
            if stats.simplified() {
                ui.label(
                    egui::RichText::new(format!("−{:.0}%", 100.0 * stats.reduction()))
                        .color(egui::Color32::from_rgb(0x7F, 0xB5, 0x8C)),
                )
                .on_hover_text(stats.simplify_line());
            }
            ui.separator();
            let step = egui::RichText::new(format!("{:.4} mm/level", stats.step_mm));
            ui.label(if stats.steps_ok() {
                step
            } else {
                step.color(egui::Color32::from_rgb(0xE0, 0xA0, 0x40))
            })
            .on_hover_text(stats.step_line());
            ui.separator();
            ui.label(stats.pitch_line()).on_hover_text(
                "Vertex spacing on the printed model — the finest detail it can carry.",
            );
            ui.separator();
            ui.label(format!("relief {}", stats.relief_line()));
            ui.separator();
            ui.label(format!(
                "{}{}",
                stats.bbox_line(),
                if stats.framed { "" } else { " · no frame" }
            ));
        });
    });
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// A slider row with the label in its own column, so the sliders line up.
fn row(
    ui: &mut egui::Ui,
    label: &str,
    tooltip: &str,
    add: impl FnOnce(&mut egui::Ui) -> egui::Response,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(LABEL_WIDTH, ui.spacing().interact_size.y),
            egui::Sense::hover(),
        );
        ui.painter().text(
            rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::TextStyle::Body.resolve(ui.style()),
            ui.visuals().text_color(),
        );
        changed = add(ui).on_hover_text(tooltip).changed();
    });
    changed
}

/// A percentage slider over a 0..1 parameter.
///
/// The depth planes are the only unitless controls left: they cut the *depth
/// map's* own range, which has no physical size until the emboss height gives it
/// one. Showing them as percentages at least says what they are a fraction of.
fn percent(ui: &mut egui::Ui, label: &str, tooltip: &str, value: &mut f32) -> bool {
    let mut shown = *value * 100.0;
    let changed = row(ui, label, tooltip, |ui| {
        ui.add(
            egui::Slider::new(&mut shown, 0.0..=100.0)
                .fixed_decimals(1)
                .suffix(" %"),
        )
    });
    if changed {
        *value = shown / 100.0;
    }
    changed
}

fn settings(ui: &mut egui::Ui, state: &mut AppState, now: f64) {
    let mut changed = false;
    let bits = state.depth.map.bits;
    // Read what the last build measured before borrowing the parameters, so the
    // grid, pitch and simplification result can be shown next to the controls
    // that set them.
    let measured = state.stats().map(|s| Measured {
        grid: s.grid,
        pitch: s.pitch_line(),
        triangles: thousands(s.triangles),
        vertices: thousands(s.vertices),
        volume_mm3: s.volume_mm3,
        filament_g: s.filament_g,
        file_line: s.file_line(),
        simplify: if s.tolerance_below_source() {
            format!(
                "blocked by the source's {:.3} mm step — raise this or pre-smooth",
                s.step_mm
            )
        } else if s.simplified() {
            format!(
                "−{:.0}% · worst deviation {:.3} mm",
                100.0 * s.reduction(),
                s.achieved_error_mm
            )
        } else {
            "every grid vertex kept".to_string()
        },
        simplify_blocked: s.tolerance_below_source(),
    });
    let p = &mut state.params;

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui
            .button("Reset")
            .on_hover_text("Back to the upstream defaults, in millimetres")
            .clicked()
        {
            *p = Params::default();
            changed = true;
        }
        if ui
            .button("Example")
            .on_hover_text("The values the original demo ships with its Einstein example")
            .clicked()
        {
            *p = Params::example();
            changed = true;
        }
    });

    ui.add_space(6.0);
    ui.heading("Depth range");
    changed |= percent(
        ui,
        "Near plane",
        "How much of the depth map's range to cut off at the front. Everything in front of it flattens to a plateau.",
        &mut p.near,
    );
    changed |= percent(
        ui,
        "Far plane",
        "How much of the depth map's range to keep. Lowering it is how you delete a busy background.",
        &mut p.far,
    );
    changed |= row(
        ui,
        "Emboss height",
        "How far the relief stands out of its plate. Independent of the printed size: changing it does not change how wide the plaque prints.",
        |ui| {
            ui.add(
                egui::Slider::new(&mut p.emboss_mm, EMBOSS_MM_MIN..=EMBOSS_MM_MAX)
                    .fixed_decimals(1)
                    .step_by(0.1)
                    .suffix(" mm"),
            )
        },
    );
    changed |= row(
        ui,
        "Invert",
        "For depth maps where near is bright. This pipeline reads a bright pixel as far away, which is what upstream does; tools that output disparity use the opposite convention.",
        |ui| ui.checkbox(&mut p.invert, ""),
    );

    ui.add_space(6.0);
    ui.heading("Size and detail");
    changed |= row(
        ui,
        "Printed size",
        "Longest outer side of the finished plaque, frame included.",
        |ui| {
            ui.add(
                egui::Slider::new(&mut p.size_mm, SIZE_MM_MIN..=SIZE_MM_MAX)
                    .fixed_decimals(0)
                    .step_by(1.0)
                    .suffix(" mm"),
            )
        },
    );
    changed |= row(
        ui,
        "Mesh resolution",
        "How finely the depth map is sampled: it is resampled so its longest side is this many pixels, and every pixel becomes one vertex. Triangle count is roughly 2·w·h, so this is the number that decides whether a slicer copes.",
        |ui| {
            ui.add(
                egui::Slider::new(&mut p.size_px, SIZE_PX_MIN..=rebuild::export_max_px())
                    .step_by(256.0)
                    .suffix(" px"),
            )
        },
    );
    if let Some(m) = &measured {
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH);
            let note = format!("{} × {} · {}", m.grid.0, m.grid.1, m.pitch);
            let note = if p.size_px > SIZE_PX_UPSTREAM_MAX {
                format!("{note} · past the original's 1024 px ceiling")
            } else {
                note
            };
            ui.label(egui::RichText::new(note).small().weak());
        });
    }
    changed |= row(
        ui,
        "Simplify",
        "Largest deviation the simplified surface may have from the depth map. A relief is mostly smooth, so a tolerance well under a layer height still removes most of the triangles. Zero keeps every grid vertex.",
        |ui| {
            ui.add(
                egui::Slider::new(&mut p.tolerance_mm, 0.0..=TOLERANCE_MM_MAX)
                    .fixed_decimals(3)
                    .step_by(0.005)
                    .suffix(" mm"),
            )
        },
    );
    if let Some(m) = &measured {
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH);
            let text = egui::RichText::new(&m.simplify).small();
            ui.label(if m.simplify_blocked {
                text.color(egui::Color32::from_rgb(0xE0, 0xA0, 0x40))
            } else {
                text.weak()
            });
        });
    }
    changed |= row(
        ui,
        "Smooth radius",
        "Median filter window, in pixels of the mesh grid. Median, not blur: it removes depth spikes and JPEG ringing without rounding off real edges.",
        |ui| {
            ui.add(
                egui::Slider::new(&mut p.filter_size, 1..=5)
                    .step_by(2.0)
                    .suffix(" px"),
            )
        },
    );
    let eight_bit = bits == BitDepth::Eight;
    ui.add_enabled_ui(eight_bit, |ui| {
        changed |= row(
            ui,
            "Pre-smooth",
            if eight_bit {
                "Gaussian blur applied before the median, to break up the 256-level staircase an 8-bit source leaves behind."
            } else {
                "Only applies to 8-bit sources. This map is 16-bit, which has no staircase worth breaking."
            },
            |ui| {
                ui.add(
                    egui::Slider::new(&mut p.presmooth, 0.0..=2.0)
                        .fixed_decimals(1)
                        .suffix(" px"),
                )
            },
        );
    });

    ui.add_space(6.0);
    ui.heading("Frame");
    changed |= row(
        ui,
        "Thickness",
        "Width of the bezel around the relief. Zero removes the frame completely — the relief's sides then drop straight to the back plate.",
        |ui| {
            ui.add(
                egui::Slider::new(&mut p.frame_thickness_mm, 0.0..=50.0)
                    .fixed_decimals(1)
                    .step_by(0.5)
                    .suffix(" mm"),
            )
        },
    );
    let framed = p.framed();
    if !framed {
        ui.horizontal(|ui| {
            ui.add_space(LABEL_WIDTH);
            ui.label(
                egui::RichText::new("no frame — sides drop straight to the plate")
                    .small()
                    .weak(),
            );
        });
    }
    ui.add_enabled_ui(framed, |ui| {
        changed |= row(
            ui,
            "Face height",
            if framed {
                "How far the frame's face stands above the front of the relief. Negative sinks it in; it can never pass the deepest point."
            } else {
                "Needs a frame. Give Thickness a width and this sets how far its face stands above the relief."
            },
            |ui| {
                ui.add(
                    egui::Slider::new(&mut p.frame_near_mm, -50.0..=50.0)
                        .fixed_decimals(1)
                        .step_by(0.5)
                        .suffix(" mm"),
                )
            },
        );
    });
    changed |= row(
        ui,
        "Back plate",
        "Thickness of the plate behind the deepest point — the part that sits on the bed.",
        |ui| {
            ui.add(
                egui::Slider::new(&mut p.frame_back_mm, 0.2..=25.0)
                    .fixed_decimals(1)
                    .step_by(0.2)
                    .suffix(" mm"),
            )
        },
    );

    ui.add_space(6.0);
    ui.heading("Printing");
    changed |= row(
        ui,
        "Orientation",
        "Face up prints it as a plaque. Upright stands it on edge, which is what a lithophane wants: the relief gradient then runs across layers instead of along them.",
        |ui| {
            let mut response =
                ui.selectable_value(&mut p.orientation, Orientation::FaceUp, "Face up");
            response |= ui.selectable_value(&mut p.orientation, Orientation::Upright, "Upright");
            response
        },
    );
    changed |= row(
        ui,
        "Layer height",
        "Your printer's layer height. Not geometry — it decides whether the depth map's quantisation gets a warning.",
        |ui| {
            ui.add(
                egui::Slider::new(&mut p.layer_mm, 0.02..=0.4)
                    .fixed_decimals(2)
                    .suffix(" mm"),
            )
        },
    );
    changed |= row(
        ui,
        "Export units",
        "The unit the exported file is written in. STL, OBJ and PLY store bare numbers with no unit field, so choosing inches divides the coordinates; slicers assume millimetres. Everything on screen stays metric either way.",
        |ui| {
            let mut response = ui.selectable_value(&mut p.units, Units::Mm, "mm");
            response |= ui.selectable_value(&mut p.units, Units::In, "in");
            response
        },
    );

    ui.add_space(6.0);
    ui.heading("View");
    row(
        ui,
        "Show chunks",
        "Tint each preview chunk separately. The surface is uploaded as several meshes so no single GPU buffer has to hold it all; this is what that looks like.",
        |ui| ui.checkbox(&mut state.show_tiles, ""),
    );

    if changed {
        state.params.clamp();
        // Far below near leaves nothing to mesh. Nudge rather than refuse: the
        // slider that just moved is the one the person is looking at.
        if state.params.far <= state.params.near {
            state.params.far = (state.params.near + 0.001).min(1.0);
            state.params.near = state.params.far - 0.001;
        }
        state.touch(now);
    }

    ui.add_space(10.0);
    if let Some(m) = measured {
        ui.separator();
        ui.label(
            egui::RichText::new(format!(
                "{} triangles · {} vertices\n{:.1} cm³ ≈ {:.0} g of PLA\n{}",
                m.triangles,
                m.vertices,
                m.volume_mm3 / 1000.0,
                m.filament_g,
                m.file_line,
            ))
            .small()
            .weak(),
        );
        if !state.format.carries_colour() && state.photo.is_some() {
            ui.label(
                egui::RichText::new("STL cannot store colour — export PLY to keep the photo")
                    .small()
                    .color(egui::Color32::from_rgb(0xE0, 0xA0, 0x40)),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Depth and photo panes
// ---------------------------------------------------------------------------

fn panes(ui: &mut egui::Ui, state: &mut AppState, window_height: f32) {
    let width = ui.available_width();
    let pane_height = ((window_height - 200.0) / 2.0).clamp(90.0, 260.0);

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Depth");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(state.depth.map.bits.label())
                    .small()
                    .weak(),
            );
        });
    });

    let texture = depth_texture(ui.ctx(), state);
    let size = egui::vec2(width, pane_height);
    match texture {
        Some(handle) => {
            ui.add(
                egui::Image::new(&handle)
                    .fit_to_exact_size(size)
                    .corner_radius(2.0),
            )
            .on_hover_text(
                "The conditioned height field — after inversion, smoothing and the near/far window. \
                 This is exactly what becomes geometry.",
            );
        }
        None => {
            ui.allocate_space(size);
        }
    }
    ui.label(egui::RichText::new(&state.depth.name).small().monospace());
    let map = &state.depth.map;
    ui.label(
        egui::RichText::new(format!(
            "{}×{} · {} levels{}",
            map.width,
            map.height,
            thousands(map.bits.levels() as usize),
            if map.was_colour { " · colour" } else { "" }
        ))
        .small()
        .weak(),
    );

    ui.add_space(10.0);
    ui.label("Image (optional)");
    match (&state.photo, &state.photo_texture) {
        (Some(photo), _) => {
            ui.label(egui::RichText::new(&photo.name).small().monospace());
            ui.label(
                egui::RichText::new(format!(
                    "{}×{} · baked as vertex colours",
                    photo.width, photo.height
                ))
                .small()
                .weak(),
            );
            if ui.button("Remove").clicked() {
                state.photo = None;
                state.photo_texture = None;
                state.photo_dirty = true;
            }
        }
        (None, _) => {
            ui.label(
                egui::RichText::new("No photo. The relief is plaster white.")
                    .small()
                    .weak(),
            );
        }
    }

    ui.add_space(10.0);
    ui.separator();
    if let Some(where_from) = platform::remembered_location() {
        ui.label(
            egui::RichText::new(format!("Settings are remembered in {where_from}"))
                .small()
                .weak(),
        );
    }
}

/// Build (or reuse) the grayscale image of the conditioned field.
fn depth_texture(ctx: &egui::Context, state: &mut AppState) -> Option<egui::TextureHandle> {
    if let Some(handle) = &state.depth_texture {
        return Some(handle.clone());
    }
    let built = state.built.as_ref()?;
    let field = &built.field;
    let depth = field.depth().max(f32::EPSILON);

    let pixels = field
        .z
        .iter()
        .map(|&z| {
            // Shown the way the *source file* reads, not the way the geometry
            // is signed: this pipeline (like upstream) treats a bright pixel as
            // far, so bright here means recessed there. Re-inverting for the
            // pane would make it disagree with the file it came from.
            let t = -z / depth;
            let v = (t.clamp(0.0, 1.0) * 255.0) as u8;
            egui::Color32::from_gray(v)
        })
        .collect::<Vec<_>>();

    let image = egui::ColorImage {
        size: [field.w, field.h],
        pixels,
        source_size: egui::vec2(field.w as f32, field.h as f32),
    };
    let handle = ctx.load_texture("depth", image, egui::TextureOptions::LINEAR);
    state.depth_texture = Some(handle.clone());
    Some(handle)
}

// ---------------------------------------------------------------------------
// The 3D view
// ---------------------------------------------------------------------------

fn view_area(
    ui: &mut egui::Ui,
    state: &AppState,
    viewport: &mut Viewport,
    texture: Option<egui::TextureId>,
) {
    ui.horizontal(|ui| {
        ui.label("3D preview");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new("drag to orbit · scroll to zoom · right-drag to pan")
                    .small()
                    .weak(),
            );
        });
    });

    let available = ui.available_size();
    let Some(texture) = texture else {
        ui.allocate_space(available);
        return;
    };

    // The texture is sized in physical pixels; the rect is in points.
    let scale = ui.ctx().pixels_per_point();
    viewport.desired = UVec2::new(
        (available.x * scale).max(16.0) as u32,
        (available.y * scale).max(16.0) as u32,
    );

    let response =
        ui.add(egui::Image::new((texture, available)).sense(egui::Sense::click_and_drag()));
    viewport.hovered = response.hovered() || response.dragged();

    if state.built.is_none() {
        ui.label(egui::RichText::new("building…").weak());
    }
}

/// 1234567 as "1,234,567".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::thousands;

    #[test]
    fn grouping_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(25_151_490), "25,151,490");
    }
}
