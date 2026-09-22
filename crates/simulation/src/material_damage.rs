//! Coarse attached-voxel response. Momentum has already been reacted into the grid;
//! attachment failure releases a body at rest, rather than reusing that impulse.
use super::DamageState;
use glam::{IVec3, Vec3};
use gpu_physics::{TerrainContact, material};
use protocol::EditRejection;
use std::collections::HashMap;
use voxel_world::{BrushEdit, Voxel, VoxelWorld, chunk_coord, index, local_coord};

/// Fracture carve: one cell of crater feathering around the destroyed voxel.
/// Exactly 1.0 reaches only the target cell, so adjacent placed cubes survive.
const FRACTURE_DIG_RADIUS: f32 = 1.0;

pub(super) const MAX_DAMAGED_BLOCKS: usize = 65_536;

/// Server-authored inelastic tool contact: 24 J, effective mass 2 kg, 25 ms.
/// Half the dissipated work enters the target; the other half enters the tool.
pub(super) fn tool_contact(target: IVec3, material: u8) -> TerrainContact {
    TerrainContact {
        target: target.to_array(),
        material: material as u32,
        dissipated_energy: 12.0,
        force: (2.0_f32 * 2.0 * 24.0).sqrt() / 0.025,
        area: 0.01,
    }
}

#[derive(Debug)]
pub(super) struct Response {
    pub damage: f32,
    pub destroyed: bool,
    pub detached: bool,
}

/// One square metre per occupied neighboring face, without explicit bond state.
pub(super) fn attached_area(world: &VoxelWorld, target: IVec3) -> f32 {
    [
        IVec3::X,
        -IVec3::X,
        IVec3::Y,
        -IVec3::Y,
        IVec3::Z,
        -IVec3::Z,
    ]
    .into_iter()
    .filter(|offset| world.block(target + *offset) != Some(0))
    .count() as f32
}

pub(super) fn response(contact: &TerrainContact, previous: f32, attached_area: f32) -> Response {
    let material = material(contact.material);
    let damage =
        previous + material.damage_energy(contact.dissipated_energy, contact.force, contact.area);
    let destroyed = damage >= material.fracture_limit(1.0);
    Response {
        damage,
        destroyed,
        detached: !destroyed && contact.force > material.attachment_strength * attached_area,
    }
}

pub(super) struct GridContactResult {
    pub fraction: f32,
    /// Per-chunk brush edits from the destruction write, for journal + delta.
    pub destroyed_edits: Option<Vec<BrushEdit>>,
}

/// Apply damage without changing voxel revisions until material destruction. A
/// rejected transaction leaves the old damage and voxel ownership untouched.
pub(super) fn apply_to_grid(
    world: &mut VoxelWorld,
    damage: &mut HashMap<IVec3, DamageState>,
    contact: &TerrainContact,
    journal_space: bool,
) -> Result<GridContactResult, EditRejection> {
    let target = IVec3::from_array(contact.target);
    if !(1..=6).contains(&contact.material)
        || world.block(target) != Some(contact.material as u8)
        || !contact.force.is_finite()
        || contact.force < 0.0
        || !contact.area.is_finite()
        || contact.area <= 0.0
        || !contact.dissipated_energy.is_finite()
        || contact.dissipated_energy < 0.0
    {
        return Err(EditRejection::InvalidTarget);
    }
    let previous = damage.get(&target).copied();
    let result = response(
        contact,
        previous.map_or(0.0, |state| state.joules),
        attached_area(world, target),
    );
    if result.destroyed {
        if !journal_space {
            return Err(EditRejection::StorageFull);
        }
        let edits = if world.voxel(target).is_some_and(|voxel| voxel.placed) {
            // Placed cubes stay discrete: a single-cell AIR write clears
            // material, density, and the placed flag.
            let (from, to) = world
                .set_block(target, 0)
                .ok_or(EditRejection::RevisionExhausted)?;
            vec![BrushEdit {
                coord: chunk_coord(target),
                from,
                to,
                voxels: vec![(index(local_coord(target)) as u16, Voxel::placed(contact.material as u8), Voxel::AIR)],
            }]
        } else {
            // Terrain fractures carve a smooth hole through the density field.
            let edits = world.brush_dig(target.as_vec3() + Vec3::splat(0.5), FRACTURE_DIG_RADIUS);
            if edits.is_empty() {
                return Err(EditRejection::RevisionExhausted);
            }
            edits
        };
        damage.remove(&target);
        return Ok(GridContactResult {
            fraction: 1.0,
            destroyed_edits: Some(edits),
        });
    }
    let release = result.detached || previous.is_some_and(|state| state.release);
    if result.damage > 0.0 || release {
        if previous.is_none() && damage.len() >= MAX_DAMAGED_BLOCKS {
            return Err(EditRejection::StorageFull);
        }
        damage.insert(
            target,
            DamageState {
                material: contact.material as u8,
                joules: result.damage,
                release,
            },
        );
    }
    Ok(GridContactResult {
        fraction: result.damage / material(contact.material).fracture_limit(1.0),
        destroyed_edits: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concentrated_hits_accumulate_without_accumulating_attachment_load() {
        let contact = tool_contact(IVec3::ZERO, 3);
        let mut damage = 0.0;
        for _ in 0..9 {
            let result = response(&contact, damage, 1.0);
            assert!(!result.destroyed && !result.detached);
            damage = result.damage;
        }
        let result = response(&contact, damage, 1.0);
        assert!(result.destroyed);
        assert!(!result.detached);
    }

    #[test]
    fn broad_load_breaks_attachment_without_fracturing_material() {
        let mut contact = tool_contact(IVec3::ZERO, 3);
        contact.force = material(3).attachment_strength * 1.1;
        contact.area = 4.0;
        let result = response(&contact, 5.0, 1.0);
        assert!(result.detached && !result.destroyed);
        assert_eq!(result.damage, 5.0);
        assert!(!response(&contact, 5.0, 6.0).detached);
    }

    #[test]
    fn support_and_gentle_push_do_not_damage() {
        let contact = TerrainContact {
            target: [0; 3],
            material: 3,
            dissipated_energy: 10.0,
            force: 30.0,
            area: 1.0,
        };
        for _ in 0..1000 {
            let result = response(&contact, 2.0, 1.0);
            assert_eq!(result.damage, 2.0);
            assert!(!result.destroyed && !result.detached);
        }
    }

    fn stone_world() -> (VoxelWorld, IVec3) {
        let mut world = VoxelWorld::default();
        world.chunks.insert(
            IVec3::ZERO,
            std::sync::Arc::new(voxel_world::Chunk::from_runs(0, &[(32768, 0)]).unwrap()),
        );
        let target = IVec3::new(2, 2, 2);
        world.set_voxel(target, Voxel::terrain(3)).unwrap();
        world.set_voxel(target - IVec3::Y, Voxel::terrain(3)).unwrap();
        (world, target)
    }

    #[test]
    fn grid_damage_survives_eviction_and_destroys_exactly_once() {
        let (mut world, target) = stone_world();
        let mut damage = HashMap::new();
        let contact = tool_contact(target, 3);
        let chunk = world.chunks[&IVec3::ZERO].clone();
        for i in 1..10 {
            let result = apply_to_grid(&mut world, &mut damage, &contact, true).unwrap();
            assert!(result.destroyed_edits.is_none());
            assert!((result.fraction - i as f32 / 10.0).abs() < 0.0001);
            assert_eq!(world.block(target), Some(3));
            assert_eq!(world.chunks[&IVec3::ZERO].revision, 2);
            assert!(!damage[&target].release);
        }
        world.remove(IVec3::ZERO);
        world.chunks.insert(IVec3::ZERO, chunk);
        let result = apply_to_grid(&mut world, &mut damage, &contact, true).unwrap();
        let edits = result.destroyed_edits.unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!((edits[0].from, edits[0].to), (2, 3));
        assert_eq!(world.block(target), Some(0));
        assert_eq!(world.density(target), Some(voxel_world::DENSITY_AIR));
        // The fracture carve leaves the supporting voxel untouched.
        assert_eq!(world.block(target - IVec3::Y), Some(3));
        assert!(!damage.contains_key(&target));
        assert!(matches!(
            apply_to_grid(&mut world, &mut damage, &contact, true),
            Err(EditRejection::InvalidTarget)
        ));
    }

    #[test]
    fn placed_cube_destroys_single_cell_and_spares_terrain_neighbors() {
        let (mut world, target) = stone_world();
        world.set_block(target, 3).unwrap();
        let mut damage = HashMap::from([(
            target,
            DamageState {
                material: 3,
                joules: 54.0,
                release: false,
            },
        )]);
        let contact = tool_contact(target, 3);
        let result = apply_to_grid(&mut world, &mut damage, &contact, true).unwrap();
        let edits = result.destroyed_edits.unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].voxels.len(), 1);
        assert_eq!(world.voxel(target), Some(Voxel::AIR));
        assert_eq!(world.voxel(target - IVec3::Y), Some(Voxel::terrain(3)));
    }


    #[test]
    fn failed_destruction_transaction_preserves_damage_and_material() {
        let (mut world, target) = stone_world();
        let mut damage = HashMap::from([(
            target,
            DamageState {
                material: 3,
                joules: 54.0,
                release: false,
            },
        )]);
        let contact = tool_contact(target, 3);
        assert!(matches!(
            apply_to_grid(&mut world, &mut damage, &contact, false),
            Err(EditRejection::StorageFull)
        ));
        std::sync::Arc::make_mut(world.chunks.get_mut(&IVec3::ZERO).unwrap()).revision = u64::MAX;
        assert!(matches!(
            apply_to_grid(&mut world, &mut damage, &contact, true),
            Err(EditRejection::RevisionExhausted)
        ));
        assert_eq!(world.block(target), Some(3));
        assert_eq!(damage[&target].joules, 54.0);
    }

    #[test]
    fn broad_impact_latches_release_but_a_later_small_contact_adds_no_old_load() {
        let (mut world, target) = stone_world();
        let mut damage = HashMap::new();
        let mut contact = tool_contact(target, 3);
        contact.force = 1600.0;
        contact.area = 1.0;
        apply_to_grid(&mut world, &mut damage, &contact, true).unwrap();
        assert!(damage[&target].release);
        assert_eq!(damage[&target].joules, 0.0);
        contact.force = 20.0;
        apply_to_grid(&mut world, &mut damage, &contact, true).unwrap();
        assert!(damage[&target].release);
        assert_eq!(damage[&target].joules, 0.0);
    }
}
