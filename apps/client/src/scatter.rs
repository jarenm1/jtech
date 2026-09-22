//! Rendered organic scatter. The server streams one instance list per chunk;
//! each instance is a downloaded package model placed at a world transform.
//!
//! Models arrive asynchronously, so instances are held until every species a
//! chunk uses has been downloaded and its mesh handles resolved.
use std::collections::{HashMap, HashSet};

use bevy::prelude::*;
use protocol::{ScatterInstance, ScatterSpeciesInfo};

use crate::package_assets::{self, PackageAssets};

/// Mesh and material handles for every primitive of one model.
type Primitives = Vec<(Handle<Mesh>, Handle<StandardMaterial>)>;

#[derive(Resource, Default)]
pub struct ScatterWorld {
    species: Vec<ScatterSpeciesInfo>,
    /// Resolved model handles per species index, once downloaded.
    models: HashMap<u16, Primitives>,
    /// Species whose downloaded model failed to parse; never retried.
    failed: HashSet<u16>,
    /// Instances per chunk, from the `Chunk` message.
    instances: HashMap<IVec3, Vec<ScatterInstance>>,
    /// Root entities per chunk, for eviction.
    entities: HashMap<IVec3, Vec<Entity>>,
}

impl ScatterWorld {
    /// Install the species table announced in `Welcome` and drop any previous
    /// session's models and instances.
    pub fn begin_session(&mut self, commands: &mut Commands, species: Vec<ScatterSpeciesInfo>) {
        self.clear(commands);
        self.species = species;
        self.models.clear();
        self.failed.clear();
    }

    pub fn receive(&mut self, coord: IVec3, instances: Vec<ScatterInstance>) {
        self.instances.insert(coord, instances);
    }

    pub fn forget(&mut self, coord: IVec3, commands: &mut Commands) {
        self.instances.remove(&coord);
        if let Some(entities) = self.entities.remove(&coord) {
            for entity in entities {
                commands.entity(entity).despawn();
            }
        }
    }

    pub fn clear(&mut self, commands: &mut Commands) {
        for (_, entities) in self.entities.drain() {
            for entity in entities {
                commands.entity(entity).despawn();
            }
        }
        self.instances.clear();
    }

    /// Resident instance count, for metrics.
    pub fn instance_count(&self) -> usize {
        self.instances.values().map(Vec::len).sum()
    }
}

/// Resolve newly downloaded models, then spawn any chunk whose species are all
/// resident. Runs every frame; the work is bounded by pending downloads.
pub fn sync_scatter(
    mut commands: Commands,
    mut scatter: ResMut<ScatterWorld>,
    assets: Res<PackageAssets>,
    asset_server: Res<AssetServer>,
) {
    let missing: Vec<(u16, String, String)> = scatter
        .species
        .iter()
        .enumerate()
        .filter(|(index, _)| {
            let index = *index as u16;
            !scatter.models.contains_key(&index) && !scatter.failed.contains(&index)
        })
        .map(|(index, info)| (index as u16, info.package.clone(), info.model.clone()))
        .collect();
    for (index, package, model) in missing {
        let Some(uri) = assets.uri(&package, &model) else {
            continue;
        };
        let Some(bytes) = assets.bytes(&package, &model) else {
            continue;
        };
        match package_assets::gltf_primitives(&asset_server, &uri, &bytes) {
            Some(primitives) => {
                scatter.models.insert(index, primitives);
            }
            None => {
                warn!("scatter species {index} model {model} is not a readable glTF");
                scatter.failed.insert(index);
            }
        }
    }

    let ready: Vec<IVec3> = scatter
        .instances
        .keys()
        .copied()
        .filter(|coord| !scatter.entities.contains_key(coord))
        .filter(|coord| {
            scatter.instances[coord]
                .iter()
                .all(|instance| scatter.models.contains_key(&instance.species))
        })
        .collect();
    for coord in ready {
        let instances = scatter.instances[&coord].clone();
        let mut spawned = Vec::with_capacity(instances.len());
        for instance in instances {
            let Some(primitives) = scatter.models.get(&instance.species).cloned() else {
                continue;
            };
            let root = commands
                .spawn((
                    Transform {
                        translation: Vec3::new(instance.x, instance.y, instance.z),
                        rotation: Quat::from_rotation_y(instance.yaw),
                        scale: Vec3::splat(instance.scale),
                    },
                    GlobalTransform::default(),
                    Visibility::Inherited,
                ))
                .id();
            for (mesh, material) in primitives {
                commands
                    .entity(root)
                    .with_child((Mesh3d(mesh), MeshMaterial3d(material)));
            }
            spawned.push(root);
        }
        scatter.entities.insert(coord, spawned);
    }
}
