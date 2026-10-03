//! Downloaded package files. The server announces an asset manifest in the
//! Packages message; the client fetches missing files in reliable chunks and
//! exposes them to Bevy through the `pkg://` asset source registered in main.
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use bevy::prelude::*;
/// Spawn a root entity mirroring the glTF's node hierarchy: one child per
/// node carrying its authored transform, one mesh grandchild per primitive.
/// Parses the container's JSON chunk directly - loading the root `Gltf` asset
/// through the asset server deadlocks when labeled subassets of the same file
/// load concurrently, so this spawns meshes and materials via `Mesh{i}` /
/// `Primitive` / `Material{i}` labels, which resolve independently.
/// Skips the scene spawner, which panics on unregistered reflect types under
/// this client's reduced feature set. Returns `None` on unreadable or
/// malformed input. glTF units are meters - model at real-world size.
pub fn spawn_gltf(
    commands: &mut Commands,
    asset_server: &AssetServer,
    uri: &str,
    bytes: &[u8],
    transform: Transform,
) -> Option<Entity> {
    let doc = parse_gltf(bytes)?;
    let root = commands
        .spawn((transform, GlobalTransform::default(), Visibility::Inherited))
        .id();
    let mut nested: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for node in &doc.nodes {
        nested.extend(node.children.iter().copied());
    }
    for (index, node) in doc.nodes.iter().enumerate() {
        if nested.contains(&index) {
            continue;
        }
        spawn_gltf_node(commands, asset_server, uri, &doc, index, root);
    }
    Some(root)
}

/// Mesh and material handles for every primitive of a glTF, in declaration
/// order. Lets a caller instance one model many times with shared handles
/// instead of spawning a fresh node hierarchy per placement.
pub fn gltf_primitives(
    asset_server: &AssetServer,
    uri: &str,
    bytes: &[u8],
) -> Option<Vec<(Handle<Mesh>, Handle<StandardMaterial>)>> {
    let doc = parse_gltf(bytes)?;
    let mut primitives = Vec::new();
    for mesh in 0..doc.meshes.len() {
        let count = doc.meshes[mesh];
        let offset = doc.mesh_primitive_offset[mesh];
        for primitive in 0..count {
            let material = doc
                .primitive_materials
                .get(offset + primitive)
                .copied()
                .flatten();
            let mesh_handle: Handle<Mesh> = asset_server.load(format!(
                "{uri}#{}",
                bevy::gltf::GltfAssetLabel::Primitive { mesh, primitive }
            ));
            let material_handle: Handle<StandardMaterial> = match material {
                Some(index) => asset_server.load(format!(
                    "{uri}#{}",
                    bevy::gltf::GltfAssetLabel::Material {
                        index,
                        is_scale_inverted: false,
                    }
                )),
                None => Handle::default(),
            };
            primitives.push((mesh_handle, material_handle));
        }
    }
    Some(primitives)
}

fn spawn_gltf_node(
    commands: &mut Commands,
    asset_server: &AssetServer,
    uri: &str,
    doc: &GltfDoc,
    index: usize,
    parent: Entity,
) {
    let Some(node) = doc.nodes.get(index) else {
        return;
    };
    let entity = commands
        .spawn((node.transform, GlobalTransform::default(), Visibility::Inherited))
        .id();
    commands.entity(parent).add_child(entity);
    if let Some(mesh) = node.mesh {
        let primitive_count = doc.meshes.get(mesh).copied().unwrap_or(0);
        let material_offset = doc.mesh_primitive_offset.get(mesh).copied().unwrap_or(0);
        for primitive in 0..primitive_count {
            let material = doc
                .primitive_materials
                .get(material_offset + primitive)
                .copied()
                .flatten();
            let mesh_handle: Handle<Mesh> = asset_server.load(format!(
                "{uri}#{}",
                bevy::gltf::GltfAssetLabel::Primitive { mesh, primitive }
            ));
            let material_handle: Handle<StandardMaterial> = match material {
                Some(index) => asset_server.load(format!(
                    "{uri}#{}",
                    bevy::gltf::GltfAssetLabel::Material {
                        index,
                        is_scale_inverted: false,
                    }
                )),
                None => Handle::default(),
            };
            commands.entity(entity).with_child((
                Mesh3d(mesh_handle),
                MeshMaterial3d(material_handle),
            ));
        }
    }
    for &child in &node.children {
        spawn_gltf_node(commands, asset_server, uri, doc, child, entity);
    }
}

/// Minimal view of a glTF container: nodes with transforms and mesh links,
/// meshes as primitive counts, primitives' material indices.
struct GltfDoc {
    nodes: Vec<GltfNodeDoc>,
    /// Primitive count per mesh index.
    meshes: Vec<usize>,
    /// `materials[mesh][primitive]` flattened: material index per primitive in
    /// declaration order across all meshes.
    primitive_materials: Vec<Option<usize>>,
    /// Cursor into `primitive_materials` per mesh.
    mesh_primitive_offset: Vec<usize>,
}

struct GltfNodeDoc {
    transform: Transform,
    mesh: Option<usize>,
    children: Vec<usize>,
}

fn parse_gltf(bytes: &[u8]) -> Option<GltfDoc> {
    // GLB: 12-byte header, then the JSON chunk (length + "JSON" + payload).
    if bytes.len() < 20 || &bytes[..4] != b"glTF" {
        return None;
    }
    let json_len = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    if &bytes[16..20] != b"JSON" || bytes.len() < 20 + json_len {
        return None;
    }
    let doc: serde_json::Value =
        serde_json::from_slice(&bytes[20..20 + json_len]).ok()?;

    let mut primitive_materials = Vec::new();
    let mut mesh_primitive_offset = Vec::new();
    let mut meshes = Vec::new();
    for mesh in doc["meshes"].as_array().into_iter().flatten() {
        mesh_primitive_offset.push(primitive_materials.len());
        let primitives = mesh["primitives"].as_array();
        meshes.push(primitives.map_or(0, Vec::len));
        for primitive in primitives.into_iter().flatten() {
            primitive_materials.push(primitive["material"].as_u64().map(|i| i as usize));
        }
    }

    let nodes = doc["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|node| {
            // The `Primitive`/`Material` labeled loads convert mesh data into
            // Bevy's frame (glTF -Z forward -> Bevy +Z); mirror that on node
            // transforms so hierarchy math stays consistent.
            let transform = if let Some(matrix) = node["matrix"].as_array() {
                let matrix: Vec<f32> = matrix
                    .iter()
                    .filter_map(|value| value.as_f64().map(|v| v as f32))
                    .collect();
                if matrix.len() == 16 {
                    let mirror = Mat4::from_scale(Vec3::new(-1.0, 1.0, -1.0));
                    Transform::from_matrix(
                        mirror
                            * Mat4::from_cols_array(&matrix.try_into().unwrap())
                            * mirror,
                    )
                } else {
                    Transform::IDENTITY
                }
            } else {
                // glTF right = -X, forward = +Z; convert to Bevy (+X, -Z) by
                // negating x/z of translation and x/z of the quaternion.
                let translation = vec3(node.get("translation"))
                    .map(|v| Vec3::new(-v.x, v.y, -v.z))
                    .unwrap_or(Vec3::ZERO);
                let rotation = quat(node.get("rotation"))
                    .map(|q| Quat::from_xyzw(-q.x, q.y, -q.z, q.w))
                    .unwrap_or(Quat::IDENTITY);
                let scale = vec3(node.get("scale")).unwrap_or(Vec3::ONE);
                Transform {
                    translation,
                    rotation,
                    scale,
                }
            };
            GltfNodeDoc {
                transform,
                mesh: node["mesh"].as_u64().map(|i| i as usize),
                children: node["children"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|child| child.as_u64().map(|i| i as usize))
                    .collect(),
            }
        })
        .collect();
    Some(GltfDoc {
        nodes,
        meshes,
        primitive_materials,
        mesh_primitive_offset,
    })
}

fn vec3(value: Option<&serde_json::Value>) -> Option<Vec3> {
    let values: Vec<f32> = value?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_f64().map(|v| v as f32))
        .collect();
    (values.len() == 3).then(|| Vec3::new(values[0], values[1], values[2]))
}

fn quat(value: Option<&serde_json::Value>) -> Option<Quat> {
    // glTF stores quaternions as (x, y, z, w).
    let values: Vec<f32> = value?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_f64().map(|v| v as f32))
        .collect();
    (values.len() == 4)
        .then(|| Quat::from_xyzw(values[0], values[1], values[2], values[3]))
}

use protocol::{ClientMessage, PackageAssetInfo};

/// One in-flight download; chunks arrive reliably and in order.
struct Pending {
    total: u32,
    received: u32,
    data: Vec<u8>,
}

#[derive(Resource, Default)]
pub struct PackageAssets {
    manifest: Vec<PackageAssetInfo>,
    pending: HashMap<(String, String), Pending>,
    /// Manifest entries resolved to their local copy; `sync` rebuilds it and
    /// `receive_chunk` patches it, so `uri`/`bytes` are hash lookups instead
    /// of a manifest scan plus filesystem stat/read per call.
    resolved: HashMap<String, HashMap<String, Resolved>>,
}

/// One manifest file's local state. `file` is content-addressed by the
/// manifest hash, so cached `bytes` stay valid across manifest refreshes
/// whenever the path matches.
struct Resolved {
    file: PathBuf,
    exists: bool,
    uri: Arc<str>,
    /// Read once, then shared; `Mutex` keeps `bytes` a `&self` call.
    bytes: Mutex<Option<Arc<[u8]>>>,
}

impl PackageAssets {
    /// Directory backing the `pkg://` asset source. A temp-dir cache keeps
    /// stale-file handling trivial: content addressing makes every version a
    /// distinct path, and a fresh boot re-downloads what it needs.
    pub fn cache_dir() -> PathBuf {
        std::env::temp_dir().join("voxel-package-assets")
    }

    fn resolved(&self, package: &str, path: &str) -> Option<&Resolved> {
        self.resolved.get(package)?.get(path)
    }

    /// `pkg://` URI usable by `AssetServer`, present once the file is local.
    pub fn uri(&self, package: &str, path: &str) -> Option<Arc<str>> {
        let resolved = self.resolved(package, path)?;
        resolved.exists.then(|| resolved.uri.clone())
    }

    /// Local file bytes, present once the download has completed. Reads the
    /// file at most once per content version, then shares the cached copy.
    pub fn bytes(&self, package: &str, path: &str) -> Option<Arc<[u8]>> {
        let resolved = self.resolved(package, path)?;
        if !resolved.exists {
            return None;
        }
        let Ok(mut cached) = resolved.bytes.lock() else {
            return None;
        };
        if cached.is_none() {
            *cached = std::fs::read(&resolved.file).map(Arc::from).ok();
        }
        cached.clone()
    }

    /// Replace the manifest; returns requests for files not yet cached or
    /// already in flight.
    pub fn sync(&mut self, manifest: Vec<PackageAssetInfo>) -> Vec<ClientMessage> {
        self.manifest = manifest;
        self.pending.retain(|(package, path), _| {
            self.manifest
                .iter()
                .any(|info| &info.package == package && &info.path == path)
        });
        // Resolve every manifest entry once: local path, on-disk presence and
        // pkg:// URI, carrying cached bytes over when the content-addressed
        // file is unchanged.
        let mut resolved: HashMap<String, HashMap<String, Resolved>> = HashMap::new();
        for info in &self.manifest {
            let relative = Path::new(&info.package)
                .join(info.hash.to_string())
                .join(&info.path);
            let file = Self::cache_dir().join(&relative);
            let bytes = self
                .resolved(&info.package, &info.path)
                .filter(|old| old.file == file)
                .and_then(|old| old.bytes.lock().ok().and_then(|b| b.clone()));
            resolved
                .entry(info.package.clone())
                .or_default()
                .insert(
                    info.path.clone(),
                    Resolved {
                        exists: file.is_file(),
                        file,
                        uri: format!("pkg://{}", relative.to_string_lossy()).into(),
                        bytes: Mutex::new(bytes),
                    },
                );
        }
        self.resolved = resolved;
        let missing: Vec<(String, String, u32)> = self
            .manifest
            .iter()
            .filter(|info| {
                !self
                    .pending
                    .contains_key(&(info.package.clone(), info.path.clone()))
                    && !self
                        .resolved(&info.package, &info.path)
                        .is_some_and(|resolved| resolved.exists)
            })
            .map(|info| (info.package.clone(), info.path.clone(), info.size))
            .collect();
        missing
            .into_iter()
            .map(|(package, path, size)| {
                self.pending.insert(
                    (package.clone(), path.clone()),
                    Pending {
                        total: size,
                        received: 0,
                        data: Vec::new(),
                    },
                );
                ClientMessage::AssetRequest { package, path }
            })
            .collect()
    }

    /// Accumulate one chunk; writes the file when the transfer completes. A
    /// mismatched chunk drops the transfer - the next manifest sync retries.
    pub fn receive_chunk(
        &mut self,
        package: &str,
        path: &str,
        offset: u32,
        total: u32,
        data: &[u8],
    ) {
        let key = (package.to_string(), path.to_string());
        let Some(pending) = self.pending.get_mut(&key) else {
            return;
        };
        if offset != pending.received || total != pending.total {
            self.pending.remove(&key);
            return;
        }
        pending.data.extend_from_slice(data);
        pending.received += data.len() as u32;
        if pending.received < pending.total {
            return;
        }
        let pending = self.pending.remove(&key).unwrap();
        let Some(resolved) = self
            .resolved
            .get_mut(package)
            .and_then(|paths| paths.get_mut(path))
        else {
            return;
        };
        if let Some(dir) = resolved.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::write(&resolved.file, &pending.data) {
            Ok(()) => {
                resolved.exists = true;
                if let Ok(mut cached) = resolved.bytes.lock() {
                    *cached = Some(Arc::from(pending.data));
                }
            }
            Err(error) => warn!("package asset {}: {error}", resolved.file.display()),
        }
    }
}
