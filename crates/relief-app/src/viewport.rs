//! The 3D view: rendered to a texture, shown inside an egui panel.
//!
//! Rendering off-screen rather than carving a viewport out of the window means
//! egui owns the whole layout — panels resize freely and the 3D view can never
//! end up underneath one.
//!
//! The preview arrives as chunks (see `relief_core::tiles`), one entity per
//! chunk, which is what keeps a 4096-px relief from needing a single enormous
//! vertex buffer. Chunks are also what `Show chunks` colours in: it is the one
//! way to see a structural decision that is otherwise invisible.

use bevy::asset::RenderAssetUsages;
use bevy::camera::{RenderTarget, visibility::RenderLayers};
use bevy::mesh::{Indices, PrimitiveTopology};
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
};
use bevy_egui::{EguiGlobalSettings, EguiTextureHandle, EguiUserTextures, PrimaryEguiContext};
use bevy_panorbit_camera::{ActiveCameraData, PanOrbitCamera};
use relief_core::tiles::{TileKind, TileMesh};

use crate::state::AppState;

/// The layer the relief lives on, so the egui context camera sees none of it.
const LAYER: usize = 1;

const INITIAL_SIZE: UVec2 = UVec2::new(900, 640);
/// Beyond this the off-screen target costs more than the detail is worth.
const MAX_SIZE: u32 = 2048;
/// Ignore sub-threshold size changes so dragging a splitter does not reallocate
/// a texture every frame.
const RESIZE_THRESHOLD: u32 = 8;

/// Vertical field of view. Narrow: a bas-relief judged through a wide lens looks
/// deeper at the edges than it will print.
const FOV: f32 = 0.42; // ~24 degrees

/// Fraction of the frame the model should fill once framed.
const FILL: f32 = 0.82;

/// Chunk tints for `Show chunks`. Muted on purpose: this is a diagnostic, not a
/// paint job, and it has to stay readable against the lighting.
const CHUNK_TINTS: [Srgba; 6] = [
    Srgba::new(0.55, 0.62, 0.72, 1.0),
    Srgba::new(0.72, 0.62, 0.52, 1.0),
    Srgba::new(0.56, 0.70, 0.60, 1.0),
    Srgba::new(0.70, 0.58, 0.66, 1.0),
    Srgba::new(0.68, 0.68, 0.52, 1.0),
    Srgba::new(0.52, 0.66, 0.70, 1.0),
];

#[derive(Resource)]
pub struct Viewport {
    pub image: Handle<Image>,
    pub camera: Entity,
    /// One entity per preview chunk, replaced wholesale on each rebuild.
    pub chunks: Vec<Entity>,
    pub plain: Handle<StandardMaterial>,
    pub frame: Handle<StandardMaterial>,
    pub tints: Vec<Handle<StandardMaterial>>,
    /// The photo material, and the texture it was built around.
    textured: Option<(Handle<Image>, Handle<StandardMaterial>)>,
    pub size: UVec2,
    /// Size the UI would like; applied by [`apply_size`].
    pub desired: UVec2,
    /// Set by the UI each frame; drives whether the camera receives input.
    pub hovered: bool,
    last_generation: u64,
    /// Bounding radius the camera was last framed for.
    framed_radius: f32,
}

fn render_texture(size: UVec2) -> Image {
    let extent = Extent3d {
        width: size.x,
        height: size.y,
        depth_or_array_layers: 1,
    };
    let mut image = Image {
        texture_descriptor: TextureDescriptor {
            label: Some("relief-viewport"),
            size: extent,
            dimension: TextureDimension::D2,
            format: TextureFormat::Bgra8UnormSrgb,
            mip_level_count: 1,
            sample_count: 1,
            usage: TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_DST
                | TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        },
        ..default()
    };
    image.resize(extent);
    image
}

/// Plaster-white with a little roughness: a relief is judged by its shadows, so
/// the material should get out of the way and let the lighting do the work.
fn relief_material(base: Srgba) -> StandardMaterial {
    StandardMaterial {
        base_color: base.into(),
        perceptual_roughness: 0.75,
        reflectance: 0.06,
        ..default()
    }
}

pub fn setup(
    mut commands: Commands,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut user_textures: ResMut<EguiUserTextures>,
    mut egui_settings: ResMut<EguiGlobalSettings>,
) {
    // We supply the egui context ourselves, on a camera that renders nothing.
    egui_settings.auto_create_primary_context = false;

    let image = images.add(render_texture(INITIAL_SIZE));
    user_textures.add_image(EguiTextureHandle::Strong(image.clone()));
    let layer = RenderLayers::layer(LAYER);

    let plain = materials.add(relief_material(Srgba::new(0.82, 0.82, 0.80, 1.0)));
    let frame = materials.add(relief_material(Srgba::new(0.62, 0.63, 0.65, 1.0)));
    let tints = CHUNK_TINTS
        .iter()
        .map(|c| materials.add(relief_material(*c)))
        .collect();

    let camera = commands
        .spawn((
            Camera3d::default(),
            Camera {
                // The off-screen pass must run before the on-screen one.
                order: -1,
                clear_color: ClearColorConfig::Custom(Color::srgb(0.055, 0.067, 0.075)),
                ..default()
            },
            RenderTarget::Image(image.clone().into()),
            // Perspective, at a long lens.
            //
            // Orthographic would be the obvious choice for a shallow object,
            // but it makes `PanOrbitCamera::radius` mean "orthographic scale"
            // rather than "distance", and panorbit 0.35 does not drive that
            // scale — the model ends up filling the frame from the inside. A
            // narrow field of view gets the flat, undistorted look orthographic
            // was wanted for while leaving `radius` as a plain distance.
            Projection::from(PerspectiveProjection {
                fov: FOV,
                near: 0.1,
                far: 100_000.0,
                ..default()
            }),
            PanOrbitCamera {
                yaw: Some(-0.45),
                pitch: Some(0.55),
                ..default()
            },
            // Per-camera component in Bevy 0.19, not a resource.
            // Bevy's own default is 80; a relief needs its shadows, so this
            // stays in that neighbourhood. At 900 the whole model saturates to
            // its base colour and the geometry becomes invisible.
            AmbientLight {
                brightness: 130.0,
                ..default()
            },
            layer.clone(),
        ))
        .id();

    // Three-point lighting, keyed from the upper left. Directional rather than
    // point lights so the shading does not change when the model does: relief
    // depth is the thing being judged here.
    // Key, fill, and a low back light. Deliberately dim by daylight standards
    // (bevy's default is 10,000 lux): the relief is judged by the shadows in it,
    // and a bright key flattens exactly the shallow detail being judged.
    for (direction, illuminance) in [
        (Vec3::new(-2.0, 3.0, 2.5), 2_600.0),
        (Vec3::new(3.0, 0.5, 2.0), 900.0),
        (Vec3::new(0.0, -2.0, 1.0), 400.0),
    ] {
        commands.spawn((
            DirectionalLight {
                illuminance,
                ..default()
            },
            Transform::from_translation(direction).looking_at(Vec3::ZERO, Vec3::Y),
            layer.clone(),
        ));
    }

    // The egui context lives on its own camera which draws no 3D at all: egui
    // covers the window, and the 3D content arrives as a texture.
    commands.spawn((
        PrimaryEguiContext,
        Camera3d::default(),
        Camera {
            order: 0,
            ..default()
        },
        RenderLayers::none(),
    ));

    commands.insert_resource(Viewport {
        image,
        camera,
        chunks: Vec::new(),
        plain,
        frame,
        tints,
        textured: None,
        size: INITIAL_SIZE,
        desired: INITIAL_SIZE,
        hovered: false,
        last_generation: u64::MAX,
        framed_radius: 0.0,
    });
}

fn to_mesh(tile: &TileMesh) -> Mesh {
    Mesh::new(
        PrimitiveTopology::TriangleList,
        // The CPU copy is dropped once the chunk is uploaded. Nothing reads a
        // preview mesh back — exports rebuild from the height field — and at
        // preview resolution this is tens of megabytes.
        RenderAssetUsages::RENDER_WORLD,
    )
    .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, tile.positions.clone())
    .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, tile.normals.clone())
    .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, tile.uvs.clone())
    .with_inserted_indices(Indices::U32(tile.indices.clone()))
}

/// Replace the preview chunks when a rebuild lands.
pub fn sync_chunks(
    mut commands: Commands,
    state: Res<AppState>,
    mut viewport: ResMut<Viewport>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut cameras: Query<&mut PanOrbitCamera>,
) {
    let Some(built) = &state.built else {
        return;
    };
    // `show_tiles` changes materials rather than geometry, but it is cheap
    // enough to fold into the same path.
    let generation =
        state.generation * 4 + u64::from(state.show_tiles) * 2 + u64::from(state.params.framed());
    let photo_changed = match (&state.photo_texture, &viewport.textured) {
        (Some(handle), Some((baked, _))) => handle != baked,
        (Some(_), None) | (None, Some(_)) => true,
        (None, None) => false,
    };
    if viewport.last_generation == generation && !photo_changed {
        return;
    }
    viewport.last_generation = generation;

    if photo_changed {
        viewport.textured = state.photo_texture.as_ref().map(|handle| {
            let material = materials.add(StandardMaterial {
                // White, or the tint would fight the photo's own colours.
                base_color: Color::WHITE,
                base_color_texture: Some(handle.clone()),
                perceptual_roughness: 0.75,
                reflectance: 0.06,
                ..default()
            });
            (handle.clone(), material)
        });
    }

    for entity in viewport.chunks.drain(..) {
        commands.entity(entity).despawn();
    }

    let layer = RenderLayers::layer(LAYER);
    let surface_material = match (&viewport.textured, state.show_tiles) {
        (Some((_, material)), false) => material.clone(),
        _ => viewport.plain.clone(),
    };

    // With no bezel the sides and back plate are not a frame, they are the body
    // of the plaque; giving them their own shade would draw a border that the
    // print does not have.
    let framed = state.params.framed();
    let mut surface_index = 0usize;
    for tile in &built.tiles {
        let material = match tile.kind {
            TileKind::Frame if framed => viewport.frame.clone(),
            TileKind::Frame => viewport.plain.clone(),
            TileKind::Surface => {
                let material = if state.show_tiles {
                    viewport.tints[surface_index % viewport.tints.len()].clone()
                } else {
                    surface_material.clone()
                };
                surface_index += 1;
                material
            }
        };
        let entity = commands
            .spawn((
                Mesh3d(meshes.add(to_mesh(tile))),
                MeshMaterial3d(material),
                Transform::IDENTITY,
                layer.clone(),
            ))
            .id();
        viewport.chunks.push(entity);
    }

    // Reframe only when the subject's size has really changed. Refitting on
    // every rebuild would yank the view back while someone is orbiting it.
    let extents = built.stats.bbox_mm;
    let radius = 0.5 * (extents[0].powi(2) + extents[1].powi(2) + extents[2].powi(2)).sqrt();
    let changed = (radius - viewport.framed_radius).abs() > 0.05 * radius.max(1.0);
    if changed {
        viewport.framed_radius = radius;
        if let Ok(mut orbit) = cameras.get_mut(viewport.camera) {
            let centre = Vec3::new(0.0, 0.0, -0.5 * extents[2]);
            // Distance at which a sphere of `radius` fills `FILL` of the frame.
            let distance = radius / (FOV * 0.5).tan() / FILL;
            orbit.focus = centre;
            orbit.target_focus = centre;
            // Assigning both skips panorbit's smoothing, so a refit snaps
            // rather than drifting into place.
            orbit.radius = Some(distance);
            orbit.target_radius = distance;
            // Without this the camera never moves. panorbit writes the
            // transform only when it detects movement — a difference between a
            // value and its target, or an input — so assigning both a value and
            // its target, which is what makes a refit snap instead of easing,
            // reads as "nothing changed" and the transform is left where it
            // was. Which, before anything has framed the model, is the origin:
            // inside the relief, looking at the back of it.
            orbit.force_update = true;
        }
    }
}

/// Resize the off-screen texture to match the panel showing it.
pub fn apply_size(mut viewport: ResMut<Viewport>, mut images: ResMut<Assets<Image>>) {
    let desired = viewport
        .desired
        .clamp(UVec2::splat(16), UVec2::splat(MAX_SIZE));
    let delta = desired.as_ivec2() - viewport.size.as_ivec2();
    if delta.x.unsigned_abs() < RESIZE_THRESHOLD && delta.y.unsigned_abs() < RESIZE_THRESHOLD {
        return;
    }
    if let Some(mut image) = images.get_mut(&viewport.image) {
        image.resize(Extent3d {
            width: desired.x,
            height: desired.y,
            depth_or_array_layers: 1,
        });
        viewport.size = desired;
    }
}

/// Only let the orbit camera see the mouse while the pointer is over the view.
///
/// `ActiveCameraData` is a single resource and `manual: true` tells the plugin
/// to leave it to us. Clearing `entity` when nothing is hovered stops a drag
/// over the settings panel from spinning the model.
pub fn route_camera_input(
    viewport: Res<Viewport>,
    windows: Query<&Window>,
    mut active: ResMut<ActiveCameraData>,
) {
    let window_size = windows
        .iter()
        .next()
        .map(|w| Vec2::new(w.width(), w.height()))
        .unwrap_or(Vec2::ONE);

    active.set_if_neq(ActiveCameraData {
        entity: viewport.hovered.then_some(viewport.camera),
        viewport_size: viewport.hovered.then_some(viewport.size.as_vec2()),
        window_size: Some(window_size),
        manual: true,
    });
}
