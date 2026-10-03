//! Procedural sky dome driven by the day cycle in [`crate::lighting`].
//!
//! A camera-centred sphere renders `sky.wgsl` behind all terrain: a
//! horizon-to-zenith gradient, a sun glow and disk, a moon disk, and a star
//! field. The flat [`ClearColor`] stays as the fallback for the frame before
//! the dome spawns.
use bevy::{
    asset::embedded_asset,
    light::{NotShadowCaster, NotShadowReceiver},
    mesh::MeshVertexBufferLayoutRef,
    pbr::{Material, MaterialPipeline, MaterialPipelineKey, MaterialPlugin},
    prelude::*,
    render::render_resource::{AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError},
    shader::ShaderRef,
};

use crate::lighting::{DayCycle, Lighting, sample};

pub struct SkyPlugin;

impl Plugin for SkyPlugin {
    fn build(&self, app: &mut App) {
        embedded_asset!(app, "apps/client/src", "sky.wgsl");
        app.add_plugins(MaterialPlugin::<SkyMaterial> {
            prepass_enabled: false,
            shadows_enabled: false,
            ..default()
        })
        .add_systems(Update, (spawn_dome, update_sky));
    }
}

/// Procedural sky material. Uniforms are packed four-to-a-vector; see
/// `sky.wgsl` for the layout.
#[derive(Asset, AsBindGroup, Reflect, Debug, Clone)]
struct SkyMaterial {
    /// xyz = direction toward the sun, w = sun disk visibility.
    #[uniform(0)]
    sun: Vec4,
    /// rgb = linear sun tint, a = glow strength.
    #[uniform(1)]
    sun_color: Vec4,
    /// rgb = linear horizon color, a = star intensity.
    #[uniform(2)]
    horizon: Vec4,
    /// rgb = linear zenith color, a = moon visibility.
    #[uniform(3)]
    zenith: Vec4,
}

impl Material for SkyMaterial {
    fn fragment_shader() -> ShaderRef {
        "embedded://voxel_client/sky.wgsl".into()
    }

    fn specialize(
        _pipeline: &MaterialPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        _key: MaterialPipelineKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        // The dome is viewed from inside, so its inward faces must survive the
        // pipeline's default back-face culling.
        descriptor.primitive.cull_mode = None;
        Ok(())
    }
}

impl SkyMaterial {
    fn from_lighting(lighting: &Lighting) -> Self {
        Self {
            sun: lighting.sun.extend(lighting.sun_visibility),
            sun_color: lighting.sun_tint.extend(lighting.sun_glow),
            horizon: lighting.horizon.extend(lighting.stars),
            zenith: lighting.zenith.extend(lighting.moon_strength),
        }
    }
}

#[derive(Component)]
struct SkyDome;

/// Spawns the dome once a 3D camera exists, parented so it follows the camera
/// without per-frame transform writes. The sphere is scaled to the camera's
/// far distance so it always sits behind the terrain it covers.
fn spawn_dome(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<SkyMaterial>>,
    cycle: Res<DayCycle>,
    cameras: Query<(Entity, &Projection), With<Camera3d>>,
    domes: Query<(), With<SkyDome>>,
) {
    if !domes.is_empty() {
        return;
    }
    let Some((camera, projection)) = cameras.iter().next() else {
        return;
    };
    let radius = match projection {
        Projection::Perspective(perspective) => perspective.far,
        _ => 4000.0,
    };
    // The sphere's flat triangles dip inside its radius by the chord sagitta,
    // so push the dome past the far distance to stay behind the farthest
    // terrain. The projection is infinite-reverse, so there is no far clip.
    let radius = radius * 1.05;
    let material = materials.add(SkyMaterial::from_lighting(&sample(cycle.hour)));
    commands.entity(camera).with_children(|parent| {
        parent.spawn((
            SkyDome,
            Mesh3d(meshes.add(Sphere::new(1.0).mesh().uv(32, 16))),
            MeshMaterial3d(material),
            Transform::from_scale(Vec3::splat(radius)),
            NotShadowCaster,
            NotShadowReceiver,
        ));
    });
}

fn update_sky(
    cycle: Res<DayCycle>,
    mut materials: ResMut<Assets<SkyMaterial>>,
    domes: Query<&MeshMaterial3d<SkyMaterial>, With<SkyDome>>,
) {
    let lighting = sample(cycle.hour);
    for dome in &domes {
        if let Some(material) = materials.get_mut(&dome.0) {
            *material = SkyMaterial::from_lighting(&lighting);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sky_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(bevy::asset::AssetPlugin::default())
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<SkyMaterial>>()
            .init_resource::<DayCycle>()
            .add_plugins(SkyPlugin);
        app
    }

    #[test]
    fn dome_spawns_parented_to_the_camera_and_tracks_the_clock() {
        let mut app = sky_app();
        let camera = app
            .world_mut()
            .spawn((
                Camera3d::default(),
                Projection::Perspective(PerspectiveProjection {
                    far: 1234.0,
                    ..default()
                }),
                Transform::default(),
            ))
            .id();
        app.update();

        let world = app.world_mut();
        let mut domes = world
            .query_filtered::<(&ChildOf, &Transform, &MeshMaterial3d<SkyMaterial>), With<SkyDome>>();
        let (parent, transform, material) = domes.iter(world).next().expect("dome spawned");
        assert_eq!(parent.parent(), camera);
        assert_eq!(transform.scale, Vec3::splat(1234.0 * 1.05));
        let handle = material.0.clone();

        world.resource_mut::<DayCycle>().hour = 0.0;
        app.update();
        let material = app
            .world()
            .resource::<Assets<SkyMaterial>>()
            .get(&handle)
            .expect("material alive");
        assert_eq!(material.zenith.w, sample(0.0).moon_strength);
        assert!(material.horizon.w > 0.9, "stars should be out at midnight");
        assert!(material.sun.w < 0.01, "sun disk should be hidden at midnight");
    }
}
