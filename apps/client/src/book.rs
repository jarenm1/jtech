//! Book view: the inventory and character UI rendered onto the pages of an
//! offscreen 3D book, shown in the pane.
//!
//! Each UI tree targets a page camera (`UiTargetCamera`), so Bevy lays it out in
//! page-texture space and renders it into that page's texture; the page quads
//! sample those textures. The book itself renders into a third texture that the
//! pane displays.
//!
//! Bevy only picks UI rendered to a window, so [`pick`] maps the pane cursor
//! through the book camera onto a page and drives `Interaction` for that page's
//! nodes itself, mirroring `ui_focus_system`.
//!
//! The page quads are procedural for now; an authored book model replaces the
//! cover and body while the pages keep their known geometry, so the cursor
//! mapping stays analytic.

use bevy::{
    camera::{RenderTarget, visibility::RenderLayers},
    image::BevyDefault,
    math::Ray3d,
    prelude::*,
    render::render_resource::TextureFormat,
    ui::{FocusPolicy, UiStack},
};

use crate::{
    inventory_ui::{InventoryPanel, InventoryUi},
    ui_theme::palette,
};

/// Page render target size (square, matching the page quads).
pub(crate) const PAGE_SIZE: UVec2 = UVec2::new(512, 512);
/// Book render target size (landscape, matching the open book).
pub(crate) const BOOK_SIZE: UVec2 = UVec2::new(1024, 512);
/// Render layer for the book scene; the preview uses 1 and item icons 2+.
const BOOK_LAYER: usize = 40;
/// Page quad size in metres.
const PAGE_W: f32 = 0.46;
const PAGE_H: f32 = 0.46;
/// Angle each page tilts away from the spine.
const PAGE_TILT: f32 = 0.16;
/// Camera framing the open book.
const BOOK_CAMERA_DISTANCE: f32 = 1.15;
const BOOK_CAMERA_FOV: f32 = 30.0;

/// Which page a quad is.
#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageSide {
    Left,
    Right,
}

/// A page's UI camera.
#[derive(Component)]
pub(crate) struct PageCamera;

/// The camera that renders the book itself.
#[derive(Component)]
pub(crate) struct BookCamera;

/// The book's render target and the page cameras the UI trees target.
#[derive(Resource)]
pub(crate) struct BookView {
    /// Texture the pane displays.
    pub texture: Handle<Image>,
    /// Page cameras, indexed by [`PageSide`].
    pub page_cameras: [Entity; 2],
}

impl BookView {
    pub(crate) fn page_camera(&self, side: PageSide) -> Entity {
        self.page_cameras[match side {
            PageSide::Left => 0,
            PageSide::Right => 1,
        }]
    }
}

/// Build the page targets, the book scene and its camera. Returns the view the
/// pane and the UI trees need.
pub(crate) fn spawn(
    commands: &mut Commands,
    images: &mut Assets<Image>,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) -> BookView {
    let layers = RenderLayers::layer(BOOK_LAYER);
    let mut page_cameras = [Entity::PLACEHOLDER; 2];
    let mut page_textures: [Handle<Image>; 2] = std::array::from_fn(|_| Handle::default());
    for index in 0..2 {
        let texture = images.add(Image::new_target_texture(
            PAGE_SIZE.x,
            PAGE_SIZE.y,
            TextureFormat::bevy_default(),
        ));
        page_textures[index] = texture.clone();
        page_cameras[index] = commands
            .spawn((
                Camera3d::default(),
                Camera {
                    target: RenderTarget::Image(texture.into()),
                    order: -4 + index as isize,
                    is_active: false,
                    clear_color: ClearColorConfig::Custom(palette::PAGE),
                    ..default()
                },
                layers.clone(),
                PageCamera,
            ))
            .id();
    }

    // Page quads: procedural, with known geometry so the cursor mapping is a
    // plane intersection rather than a mesh raycast.
    let page_mesh = meshes.add(Rectangle::new(PAGE_W, PAGE_H));
    for (index, side) in [PageSide::Left, PageSide::Right].into_iter().enumerate() {
        let sign = if side == PageSide::Left { -1.0 } else { 1.0 };
        let material = materials.add(StandardMaterial {
            base_color_texture: Some(page_textures[index].clone()),
            unlit: true,
            cull_mode: None,
            ..default()
        });
        commands.spawn((
            Mesh3d(page_mesh.clone()),
            MeshMaterial3d(material),
            Transform::from_xyz(sign * (PAGE_W / 2.0 + 0.005), 0.0, 0.0)
                .with_rotation(Quat::from_rotation_y(-sign * PAGE_TILT)),
            layers.clone(),
            side,
        ));
    }
    // Cover: a slab well behind the pages, which tilt back into the spine.
    let cover = materials.add(StandardMaterial {
        base_color: Color::srgb(0.16, 0.05, 0.05),
        perceptual_roughness: 0.8,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(PAGE_W * 2.0 + 0.05, PAGE_H + 0.05, 0.03))),
        MeshMaterial3d(cover),
        Transform::from_xyz(0.0, 0.0, -0.09),
        layers.clone(),
    ));

    let texture = images.add(Image::new_target_texture(
        BOOK_SIZE.x,
        BOOK_SIZE.y,
        TextureFormat::bevy_default(),
    ));
    commands.spawn((
        Camera3d::default(),
        Camera {
            target: RenderTarget::Image(texture.clone().into()),
            order: -2,
            is_active: false,
            clear_color: ClearColorConfig::Custom(Color::NONE),
            ..default()
        },
        Projection::Perspective(PerspectiveProjection {
            fov: BOOK_CAMERA_FOV.to_radians(),
            ..default()
        }),
        Transform::from_xyz(0.0, 0.0, BOOK_CAMERA_DISTANCE).looking_at(Vec3::ZERO, Vec3::Y),
        layers.clone(),
        BookCamera,
    ));
    // Key and fill lights so the cover reads; the pages are unlit.
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

    BookView {
        texture,
        page_cameras,
    }
}

/// Render the book only while the pane is open.
pub(crate) fn sync_cameras(
    ui: Res<InventoryUi>,
    mut cameras: Query<&mut Camera, Or<(With<PageCamera>, With<BookCamera>)>>,
) {
    for mut camera in &mut cameras {
        if camera.is_active != ui.open {
            camera.is_active = ui.open;
        }
    }
}

/// Drive `Interaction` for the page UI from the pane cursor.
#[allow(clippy::too_many_arguments)] // Mirrors `ui_focus_system`'s inputs.
pub(crate) fn pick(
    window: Single<&Window>,
    buttons: Res<ButtonInput<MouseButton>>,
    view: Res<BookView>,
    pane: Single<(&ComputedNode, &UiGlobalTransform), With<InventoryPanel>>,
    book_camera: Single<(&Camera, &GlobalTransform), With<BookCamera>>,
    pages: Query<(&PageSide, &GlobalTransform)>,
    stack: Res<UiStack>,
    mut nodes: Query<(
        &ComputedNode,
        &UiGlobalTransform,
        &ComputedUiTargetCamera,
        Option<&mut Interaction>,
        Option<&FocusPolicy>,
    )>,
) {
    let target = window.cursor_position().and_then(|cursor| {
        let (pane_node, pane_transform) = *pane;
        let local = pane_node.normalize_point(*pane_transform, cursor)?;
        if !(0.0..=1.0).contains(&local.x) || !(0.0..=1.0).contains(&local.y) {
            return None;
        }
        let viewport = Vec2::new(local.x * BOOK_SIZE.x as f32, local.y * BOOK_SIZE.y as f32);
        let (camera, camera_transform) = *book_camera;
        let ray = camera.viewport_to_world(camera_transform, viewport).ok()?;
        pages
            .iter()
            .find_map(|(side, transform)| {
                page_point(&ray, transform).map(|point| (view.page_camera(*side), point))
            })
    });

    let pressed = buttons.pressed(MouseButton::Left);
    let mut blocked = false;
    for entity in stack.uinodes.iter().rev() {
        let Ok((node, transform, camera, interaction, policy)) = nodes.get_mut(*entity) else {
            continue;
        };
        let Some(mut interaction) = interaction else {
            continue;
        };
        let hovered = !blocked
            && target.is_some_and(|(camera_entity, point)| {
                camera.get() == Some(camera_entity) && node.contains_point(*transform, point)
            });
        let desired = if hovered {
            if pressed {
                Interaction::Pressed
            } else {
                Interaction::Hovered
            }
        } else {
            Interaction::None
        };
        if *interaction != desired {
            *interaction = desired;
        }
        if hovered {
            blocked = matches!(policy.unwrap_or(&FocusPolicy::Block), FocusPolicy::Block);
        }
    }
}

/// Intersect a ray with a page quad, returning the point in page-texture space.
fn page_point(ray: &Ray3d, transform: &GlobalTransform) -> Option<Vec2> {
    let normal = transform.rotation() * Vec3::Z;
    let denominator = ray.direction.dot(normal);
    if denominator.abs() < 1e-6 {
        return None;
    }
    let distance = (transform.translation() - ray.origin).dot(normal) / denominator;
    if distance < 0.0 {
        return None;
    }
    let hit = ray.origin + *ray.direction * distance;
    let local = transform.affine().inverse().transform_point3(hit);
    if local.x.abs() > PAGE_W / 2.0 || local.y.abs() > PAGE_H / 2.0 {
        return None;
    }
    let u = local.x / PAGE_W + 0.5;
    let v = 0.5 - local.y / PAGE_H;
    Some(Vec2::new(u * PAGE_SIZE.x as f32, v * PAGE_SIZE.y as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_point_maps_quad_corners_to_texture_corners() {
        let transform = GlobalTransform::from(
            Transform::from_xyz(0.0, 0.0, 0.0).with_rotation(Quat::IDENTITY),
        );
        // Straight down -Z onto the quad centre.
        let ray = Ray3d::new(Vec3::new(0.0, 0.0, 1.0), Dir3::NEG_Z);
        let point = page_point(&ray, &transform).unwrap();
        assert!((point.x - PAGE_SIZE.x as f32 / 2.0).abs() < 0.01);
        assert!((point.y - PAGE_SIZE.y as f32 / 2.0).abs() < 0.01);

        // Top-left corner of the quad maps to the texture origin.
        let ray = Ray3d::new(
            Vec3::new(-PAGE_W / 2.0, PAGE_H / 2.0, 1.0),
            Dir3::NEG_Z,
        );
        let point = page_point(&ray, &transform).unwrap();
        assert!(point.x.abs() < 0.01, "{point:?}");
        assert!(point.y.abs() < 0.01, "{point:?}");

        // A ray missing the quad returns nothing.
        let ray = Ray3d::new(Vec3::new(PAGE_W, 0.0, 1.0), Dir3::NEG_Z);
        assert!(page_point(&ray, &transform).is_none());
    }
}
