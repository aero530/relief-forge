//! Take one screenshot and quit.
//!
//! `RELIEF_SHOT=path.png cargo run -p relief-app` renders a few frames, saves
//! the window, and exits. It exists so the app can be *looked at* without a
//! person driving it: for the README, and as the only way to catch a UI that
//! compiles, runs, and draws nothing.
//!
//! Desktop only — there is no filesystem to save into on the web.

use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};

/// Frames to let pass before the shot.
///
/// The first frame has no preview yet: the rebuild is debounced by 120 ms and
/// then runs on a worker, so a shot taken too early catches an empty viewport.
const WARMUP: u32 = 90;

#[derive(Resource)]
pub struct Shot {
    path: String,
    frame: u32,
}

/// Install the screenshot system when `RELIEF_SHOT` names a file.
pub fn plugin(app: &mut App) {
    let Ok(path) = std::env::var("RELIEF_SHOT") else {
        return;
    };
    app.insert_resource(Shot { path, frame: 0 })
        .add_systems(Last, capture);
}

fn capture(
    mut commands: Commands,
    mut shot: ResMut<Shot>,
    mut exit: MessageWriter<AppExit>,
    state: Res<crate::state::AppState>,
) {
    shot.frame += 1;
    // Wait for warmup *and* for something to actually be on screen, so this can
    // never save a picture of an empty viewport and call it success.
    if shot.frame < WARMUP || state.built.is_none() {
        return;
    }
    if shot.frame == WARMUP.max(1) || shot.frame == WARMUP + 1 {
        info!("saving screenshot to {}", shot.path);
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(shot.path.clone()));
        return;
    }
    // Give the save a couple of frames to land before tearing the app down.
    if shot.frame > WARMUP + 10 {
        exit.write(AppExit::Success);
    }
}
