//! Character preview: an offscreen 3D view of the player's current
//! representation, rendered to a texture and shown inside the inventory pane.
//!
//! The game is first-person and ships no player model yet, so the preview
//! renders the same cuboid remote players see. When an authored model lands,
//! swap the mesh in [`spawn`] — the camera, lighting, render target and UI
//! plumbing stay as they are.
//!
//! The preview camera renders on its own render layer and is only active while
//! the pane is open, so a closed pane costs nothing.

use bevy::{
    camera::{RenderTarget, visibility::RenderLayers},
    image::BevyDefault,
    prelude::*,
    render::render_resource::TextureFormat,
};
use physics::PLAYER_HEIGHT;

use crate::{inventory_ui::InventoryUi, ui_theme::palette};

/// Render layer reserved for the preview scene; the main camera stays on layer 0.
const PREVIEW_LAYER: usize = 1;
/// Render target size, portrait to match the pane's preview viewport.
const TARGET_SIZE: UVec2 = UVec2::new(256, 384);
/// Seconds per full turntable revolution.
const TURN_SECONDS: f32 = 10.0;

#[derive(Component)]
pub(crate) struct PreviewCamera;

#[derive(Component)]
pub(crate) struct PreviewModel;

/// Build the offscreen preview and return the texture the panel displays.
pub(crate) fn spawn(
    commands: &mut Commands,
    images: &mut Assets<Image>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) -> Handle<Image> {
    let target = images.add(Image::new_target_texture(
        TARGET_SIZE.x,
        TARGET_SIZE.y,
        TextureFormat::bevy_default(),
    ));

    let layer = RenderLayers::layer(PREVIEW_LAYER);

    // Frame the model from slightly above; `order: -1` keeps it off the main
    // window target, and `is_active` is driven by the panel's open state.
    commands.spawn((
        Camera3d::default(),
        Camera {
            target: RenderTarget::Image(target.clone().into()),
            order: -1,
            is_active: false,
            clear_color: ClearColorConfig::Custom(palette::PREVIEW_BG),
            ..default()
        },
        Projection::Perspective(PerspectiveProjection {
            fov: 30.0_f32.to_radians(),
            ..default()
        }),
        Transform::from_xyz(0.0, PLAYER_HEIGHT * 0.62, 4.2)
            .looking_at(Vec3::new(0.0, PLAYER_HEIGHT * 0.5, 0.0), Vec3::Y),
        layer.clone(),
        PreviewCamera,
    ));

    // Key and fill lights so the model reads without touching world lighting.
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, -0.6, -0.5, 0.0)),
        layer.clone(),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 4_000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, 2.4, -0.3, 0.0)),
        layer.clone(),
    ));

    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(0.6, PLAYER_HEIGHT, 0.6))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.62, 0.64, 0.68),
            perceptual_roughness: 0.7,
            ..default()
        })),
        Transform::from_xyz(0.0, PLAYER_HEIGHT * 0.5, 0.0),
        layer,
        PreviewModel,
    ));

    target
}

/// Activate the preview only while the pane is open and turn the model slowly.
pub(crate) fn sync(
    ui: Res<InventoryUi>,
    time: Res<Time>,
    mut camera: Single<&mut Camera, With<PreviewCamera>>,
    mut model: Single<&mut Transform, With<PreviewModel>>,
) {
    if camera.is_active != ui.open {
        camera.is_active = ui.open;
    }
    if ui.open {
        model.rotation = Quat::from_rotation_y(time.elapsed_secs() / TURN_SECONDS * std::f32::consts::TAU);
    }
}
