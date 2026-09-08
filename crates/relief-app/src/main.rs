//! Relief Forge — depth map in, printable bas-relief out.
//!
//! One binary serves both targets: `cargo run -p relief-app` for desktop,
//! `trunk serve` for the browser.
//!
//! The geometry lives in `relief-core` and knows nothing about Bevy or egui.
//! This crate is the shell around it: panels, an off-screen 3D view, file
//! dialogs that work in both places, and the debounce that keeps a slider drag
//! from rebuilding a mesh on every frame.

mod platform;
mod rebuild;
#[cfg(not(target_arch = "wasm32"))]
mod shot;
mod state;
mod ui;
mod viewport;

use bevy::prelude::*;
use bevy_egui::{EguiPlugin, EguiPrimaryContextPass};
use bevy_panorbit_camera::{PanOrbitCameraPlugin, PanOrbitCameraSystemSet};

use state::AppState;

fn main() {
    let mut app = App::new();
    app.init_resource::<AppState>()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "Relief Forge".into(),
                // Ignored on desktop; on wasm this binds to the <canvas> in
                // index.html.
                canvas: Some("#relief-canvas".into()),
                fit_canvas_to_parent: true,
                ..default()
            }),
            ..default()
        }))
        .add_plugins(EguiPlugin::default())
        .add_plugins(PanOrbitCameraPlugin)
        .add_systems(Startup, (viewport::setup, state::restore))
        .add_systems(
            Update,
            (
                // Files opened in the previous frame's egui pass resolve first,
                // so a rebuild this frame sees them.
                state::apply_opened,
                state::watch_params,
                rebuild::drive,
                rebuild::sync_photo_texture,
                viewport::sync_chunks,
                viewport::apply_size,
                // Must land before the plugin reads it, and `manual: true`
                // keeps the plugin from overwriting our choice.
                viewport::route_camera_input.before(PanOrbitCameraSystemSet),
            )
                .chain(),
        )
        .add_systems(EguiPrimaryContextPass, ui::draw)
        // In `Last` so the exit flush sees this frame's edits.
        .add_systems(Last, (state::autosave, state::autosave_on_exit).chain());

    #[cfg(not(target_arch = "wasm32"))]
    {
        app.init_resource::<rebuild::Worker>();
        // Only does anything when RELIEF_SHOT is set.
        app.add_plugins(shot::plugin);
    }

    app.run();
}
