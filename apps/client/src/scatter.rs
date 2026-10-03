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
    /// Species still awaiting their model download and parse.
    unresolved: HashSet<u16>,
    /// Species whose downloaded model failed to parse; never retried.
    failed: HashSet<u16>,
    /// Chunks held until their species resolve: how many of the chunk's
    /// species still lack a model.
    pending_species: HashMap<IVec3, usize>,
    /// Reverse index from an unresolved or failed species to the chunks
    /// waiting on it, so one resolved model wakes only its dependents.
    waiting_on: HashMap<u16, HashSet<IVec3>>,
    /// Chunks whose species are all resolved; spawned on the next sync.
    ready: Vec<IVec3>,
    /// Last `PackageAssets` version checked against the unresolved species.
    assets_version: u64,
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
        self.unresolved = (0..self.species.len() as u16).collect();
        self.failed.clear();
    }

    pub fn receive(&mut self, coord: IVec3, instances: Vec<ScatterInstance>) {
        self.unwait(coord);
        // A chunk re-received while spawned keeps its entities, so only fresh
        // chunks enter the wait bookkeeping.
        if self.entities.contains_key(&coord) {
            self.instances.insert(coord, instances);
            return;
        }
        let needed: HashSet<u16> = instances
            .iter()
            .map(|instance| instance.species)
            .filter(|index| !self.models.contains_key(index))
            .collect();
        self.instances.insert(coord, instances);
        if needed.is_empty() {
            self.ready.push(coord);
        } else {
            self.pending_species.insert(coord, needed.len());
            for index in needed {
                self.waiting_on.entry(index).or_default().insert(coord);
            }
        }
    }

    /// Drop a chunk's wait bookkeeping when its instance list is replaced or
    /// the chunk is evicted.
    fn unwait(&mut self, coord: IVec3) {
        if self.pending_species.remove(&coord).is_none() {
            return;
        }
        for chunks in self.waiting_on.values_mut() {
            chunks.remove(&coord);
        }
        self.waiting_on.retain(|_, chunks| !chunks.is_empty());
    }

    /// Species `index` resolved: every chunk waiting on it sheds one pending
    /// species and joins `ready` once none remain. A failed species never
    /// wakes, so its dependents stay blocked forever - matching a `missing`
    /// model in the old polling path.
    fn wake(&mut self, index: u16) {
        let Some(chunks) = self.waiting_on.remove(&index) else {
            return;
        };
        for coord in chunks {
            let Some(needs) = self.pending_species.get_mut(&coord) else {
                continue;
            };
            *needs -= 1;
            if *needs == 0 {
                self.pending_species.remove(&coord);
                self.ready.push(coord);
            }
        }
    }

    pub fn forget(&mut self, coord: IVec3, commands: &mut Commands) {
        self.unwait(coord);
        self.ready.retain(|queued| *queued != coord);
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
        self.pending_species.clear();
        self.waiting_on.clear();
        self.ready.clear();
        self.unresolved.clear();
    }

    /// Resident instance count, for metrics.
    pub fn instance_count(&self) -> usize {
        self.instances.values().map(Vec::len).sum()
    }
}

/// Resolve newly downloaded models, then spawn any chunk whose species are all
/// resident. Cheap per frame: unresolved species are re-checked only when the
/// package cache generation changes, and chunks spawn only when a chunk or
/// species resolves, not by rescanning every instance.
pub fn sync_scatter(
    mut commands: Commands,
    mut scatter: ResMut<ScatterWorld>,
    assets: Res<PackageAssets>,
    asset_server: Res<AssetServer>,
) {
    if assets.version() != scatter.assets_version {
        scatter.assets_version = assets.version();
        let mut resolving = Vec::new();
        for index in &scatter.unresolved {
            let Some(info) = scatter.species.get(*index as usize) else {
                continue;
            };
            if let (Some(uri), Some(bytes)) = (
                assets.uri(&info.package, &info.model),
                assets.bytes(&info.package, &info.model),
            ) {
                resolving.push((*index, uri, bytes));
            }
        }
        for (index, uri, bytes) in resolving {
            match package_assets::gltf_primitives(&asset_server, &uri, &bytes) {
                Some(primitives) => {
                    scatter.models.insert(index, primitives);
                    scatter.unresolved.remove(&index);
                    scatter.wake(index);
                }
                None => {
                    let model = scatter
                        .species
                        .get(index as usize)
                        .map_or("unknown", |info| info.model.as_str());
                    warn!("scatter species {index} model {model} is not a readable glTF");
                    scatter.unresolved.remove(&index);
                    scatter.failed.insert(index);
                }
            }
        }
    }

    for coord in std::mem::take(&mut scatter.ready) {
        // A chunk re-received after spawning keeps its entities and must not
        // spawn a second set.
        if scatter.entities.contains_key(&coord) {
            continue;
        }
        let Some(instances) = scatter.instances.get(&coord) else {
            continue;
        };
        let mut spawned = Vec::with_capacity(instances.len());
        for instance in instances {
            let Some(primitives) = scatter.models.get(&instance.species) else {
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
                    .with_child((Mesh3d(mesh.clone()), MeshMaterial3d(material.clone())));
            }
            spawned.push(root);
        }
        scatter.entities.insert(coord, spawned);
    }
}
