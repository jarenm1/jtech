//! Item icons: a small offscreen 3D render for every item that ships a package
//! model, so the inventory can mix 3D and 2D icons. Items without a model keep
//! their flat colour swatch.
//!
//! Each icon gets its own render layer, camera and light, and renders only while
//! the inventory pane is open. Icons are rebuilt when the package set changes; a
//! model that has not downloaded yet is retried each frame, so the row upgrades
//! from swatch to model as soon as the asset lands.

use std::collections::HashMap;

use bevy::{
    camera::{RenderTarget, visibility::RenderLayers},
    image::BevyDefault,
    prelude::*,
    render::render_resource::TextureFormat,
};

use crate::{
    ClientSession,
    inventory_ui::InventoryUi,
    package_assets::{self, PackageAssets},
};

/// Icon render target edge length.
pub(crate) const ICON_SIZE: u32 = 64;
/// First render layer reserved for icons; the character preview uses layer 1.
const FIRST_ICON_LAYER: usize = 2;
/// Most icons that can render at once: render layers are a 64-bit mask.
const MAX_ICONS: usize = 32;
/// Camera framing for a model authored at real-world size (glTF units are
/// metres), so a 0.2–0.5 m item fills the icon.
const CAMERA_DISTANCE: f32 = 0.9;
const CAMERA_FOV: f32 = 30.0;

/// Rendered icon textures, keyed by item id.
#[derive(Resource, Default)]
pub(crate) struct ItemIcons {
    textures: HashMap<u32, Handle<Image>>,
    /// Package generation the icons were built from.
    revision: Option<u64>,
}

impl ItemIcons {
    /// Icon texture for an item, once its model has rendered.
    pub(crate) fn texture(&self, item: u32) -> Option<&Handle<Image>> {
        self.textures.get(&item)
    }

    /// Record a rendered icon for an item.
    pub(crate) fn set(&mut self, item: u32, texture: Handle<Image>) {
        self.textures.insert(item, texture);
    }
}

/// Marks the camera, lights and model of one icon scene.
#[derive(Component)]
pub(crate) struct IconScene;

#[derive(Component)]
pub(crate) struct IconCamera;

/// Build an icon scene for every weapon that ships a model, once its asset is
/// local. Rebuilds everything when the package set changes.
pub(crate) fn sync(
    mut commands: Commands,
    session: Res<ClientSession>,
    assets: Res<PackageAssets>,
    asset_server: Res<AssetServer>,
    mut images: ResMut<Assets<Image>>,
    mut icons: ResMut<ItemIcons>,
    scenes: Query<Entity, With<IconScene>>,
) {
    let revision = session.packages.revision;
    if icons.revision != revision {
        icons.revision = revision;
        icons.textures.clear();
        for scene in &scenes {
            commands.entity(scene).despawn();
        }
    }
    for weapon in &session.packages.melee_weapons {
        if icons.textures.contains_key(&weapon.item) || icons.textures.len() >= MAX_ICONS {
            continue;
        }
        let Some(path) = weapon.model.as_deref() else {
            continue;
        };
        let (Some(uri), Some(bytes)) = (
            assets.uri(&weapon.package, path),
            assets.bytes(&weapon.package, path),
        ) else {
            continue;
        };
        let layers = RenderLayers::layer(FIRST_ICON_LAYER + icons.textures.len());
        let target = images.add(Image::new_target_texture(
            ICON_SIZE,
            ICON_SIZE,
            TextureFormat::bevy_default(),
        ));
        commands.spawn((
            Camera3d::default(),
            Camera {
                target: RenderTarget::Image(target.clone().into()),
                order: -2,
                is_active: false,
                // Transparent so the row's colour swatch shows behind the model.
                clear_color: ClearColorConfig::Custom(Color::NONE),
                ..default()
            },
            Projection::Perspective(PerspectiveProjection {
                fov: CAMERA_FOV.to_radians(),
                ..default()
            }),
            Transform::from_xyz(0.0, 0.0, CAMERA_DISTANCE).looking_at(Vec3::ZERO, Vec3::Y),
            layers.clone(),
            IconScene,
            IconCamera,
        ));
        commands.spawn((
            DirectionalLight {
                illuminance: 12_000.0,
                shadows_enabled: false,
                ..default()
            },
            Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, -0.6, -0.5, 0.0)),
            layers.clone(),
            IconScene,
        ));
        commands.spawn((
            DirectionalLight {
                illuminance: 4_000.0,
                shadows_enabled: false,
                ..default()
            },
            Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, 2.4, -0.3, 0.0)),
            layers.clone(),
            IconScene,
        ));
        if let Some(model) = package_assets::spawn_gltf(
            &mut commands,
            &asset_server,
            &uri,
            &bytes,
            Transform::IDENTITY,
            Some(&layers),
        ) {
            commands.entity(model).insert(IconScene);
        }
        icons.set(weapon.item, target);
    }
}

/// Preview render target edge length.
pub(crate) const PREVIEW_SIZE: u32 = 256;
/// Render layer for the item preview scene.
const PREVIEW_LAYER: usize = 41;

/// Rendered preview of one item's model, shown beside the inventory list.
#[derive(Resource)]
pub(crate) struct ItemPreview {
    pub texture: Handle<Image>,
    model: Option<Entity>,
    item: Option<u32>,
}

/// Item the preview should show; set from the inventory's hover handling.
#[derive(Resource, Default)]
pub(crate) struct PreviewItem(pub Option<u32>);

/// Build the preview target and its camera.
pub(crate) fn spawn_preview(commands: &mut Commands, images: &mut Assets<Image>) -> ItemPreview {
    let texture = images.add(Image::new_target_texture(
        PREVIEW_SIZE,
        PREVIEW_SIZE,
        TextureFormat::bevy_default(),
    ));
    let layers = RenderLayers::layer(PREVIEW_LAYER);
    commands.spawn((
        Camera3d::default(),
        Camera {
            target: RenderTarget::Image(texture.clone().into()),
            order: -3,
            is_active: false,
            clear_color: ClearColorConfig::Custom(Color::NONE),
            ..default()
        },
        Projection::Perspective(PerspectiveProjection {
            fov: CAMERA_FOV.to_radians(),
            ..default()
        }),
        Transform::from_xyz(0.0, 0.0, CAMERA_DISTANCE).looking_at(Vec3::ZERO, Vec3::Y),
        layers.clone(),
        IconCamera,
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, -0.6, -0.5, 0.0)),
        layers.clone(),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 4_000.0,
            shadows_enabled: false,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(EulerRot::YXZ, 2.4, -0.3, 0.0)),
        layers,
    ));
    ItemPreview {
        texture,
        model: None,
        item: None,
    }
}

/// Swap the preview's model when the hovered item changes.
pub(crate) fn sync_preview(
    mut commands: Commands,
    session: Res<ClientSession>,
    assets: Res<PackageAssets>,
    asset_server: Res<AssetServer>,
    wanted: Res<PreviewItem>,
    mut preview: ResMut<ItemPreview>,
) {
    if preview.item == wanted.0 {
        return;
    }
    preview.item = wanted.0;
    if let Some(model) = preview.model.take() {
        commands.entity(model).despawn();
    }
    let Some(item) = wanted.0 else {
        return;
    };
    let Some(weapon) = session
        .packages
        .melee_weapons
        .iter()
        .find(|weapon| weapon.item == item)
    else {
        return;
    };
    let Some(path) = weapon.model.as_deref() else {
        return;
    };
    let (Some(uri), Some(bytes)) = (
        assets.uri(&weapon.package, path),
        assets.bytes(&weapon.package, path),
    ) else {
        return;
    };
    let layers = RenderLayers::layer(PREVIEW_LAYER);
    if let Some(model) = package_assets::spawn_gltf(
        &mut commands,
        &asset_server,
        &uri,
        &bytes,
        Transform::IDENTITY,
        Some(&layers),
    ) {
        preview.model = Some(model);
    }
}

/// Render icons only while the inventory pane is open.
pub(crate) fn sync_cameras(
    ui: Res<InventoryUi>,
    mut cameras: Query<&mut Camera, With<IconCamera>>,
) {
    for mut camera in &mut cameras {
        if camera.is_active != ui.open {
            camera.is_active = ui.open;
        }
    }
}
