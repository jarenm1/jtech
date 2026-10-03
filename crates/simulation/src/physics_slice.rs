//! Body lifecycle and asynchronous submission, independent of terrain residency.
//! CPU copies are observations; only lifecycle edits and external loads replace them.
use super::active_terrain::{self, ActiveTerrain};
use glam::{IVec3, Vec3};
use gpu_physics::{Body, GpuPhysics, PlayerCollider, TerrainContact};
use protocol::{MAX_PHYSICS_BODIES, PhysicsBodySnapshot};
use std::collections::HashSet;
use voxel_world::{CHUNK_SIZE, MAX_CHUNK_Y, MIN_CHUNK_Y, VoxelWorld, chunk_coord};

pub(super) struct PhysicsSlice {
    // wgpu map completion uses an mpsc Receiver, so protect it for Bevy's Sync bound.
    gpu: parking_lot::Mutex<GpuPhysics>,
    terrain: ActiveTerrain,
    bodies: Vec<Body>,
    ids: Vec<u32>,
    origins: Vec<IVec3>,
    next_id: u32,
    impulses: Vec<(u32, [f32; 3])>,
    players: Vec<PlayerCollider>,
    bodies_dirty: bool,
    dirty_cells: HashSet<IVec3>,
    contacts: Vec<TerrainContact>,
    failed: bool,
    tick: u64,
    /// Advances on completed GPU observations and lifecycle changes, not wall ticks.
    pub revision: u64,
    submitted_at: Option<std::time::Instant>,
    pub completions: u64,
    pub completion_us_total: u64,
    pub completion_us_max: u64,
    pub submissions: u64,
    pub busy_ticks: u64,
    pub readback_bytes: u64,
    pub terrain_upload_bytes: u64,
    pub player_upload_bytes: u64,
    pub terrain_wait_ticks: u64,
}

impl PhysicsSlice {
    pub fn new() -> Result<Self, String> {
        let gpu = GpuPhysics::new_sparse()?;
        eprintln!(
            "GPU physics: {} active_chunks={} body_cap={} step_hz=120 readback_hz=20",
            gpu.adapter_name,
            gpu_physics::MAX_TERRAIN_CHUNKS,
            MAX_PHYSICS_BODIES
        );
        Ok(Self {
            gpu: parking_lot::Mutex::new(gpu),
            terrain: ActiveTerrain::default(),
            bodies: Vec::new(),
            ids: Vec::new(),
            origins: Vec::new(),
            next_id: 1,
            impulses: Vec::new(),
            players: Vec::new(),
            bodies_dirty: false,
            dirty_cells: Default::default(),
            failed: false,
            contacts: Vec::new(),
            tick: 0,
            revision: 0,
            submitted_at: None,
            completions: 0,
            completion_us_total: 0,
            completion_us_max: 0,
            submissions: 0,
            busy_ticks: 0,
            readback_bytes: 0,
            terrain_upload_bytes: 0,
            player_upload_bytes: 0,
            terrain_wait_ticks: 0,
        })
    }

    /// Pin collision halos independently of player interest, and source cells for recovery.
    /// `out` is cleared so callers can keep a resident set allocated across ticks.
    pub fn needed_chunks(&self, out: &mut HashSet<IVec3>) {
        active_terrain::needed_chunks(&self.bodies, out);
        out.extend(self.origins.iter().copied().map(chunk_coord));
    }

    pub fn resident_chunks(&self) -> usize {
        self.terrain.resident_count()
    }

    fn valid_target(target: IVec3) -> bool {
        target.y > MIN_CHUNK_Y * CHUNK_SIZE
            && target.y < (MAX_CHUNK_Y + 1) * CHUNK_SIZE
            && target.x.unsigned_abs() < 31_999_900
            && target.z.unsigned_abs() < 31_999_900
    }

    pub fn is_busy(&self) -> bool {
        self.gpu.lock().is_busy()
    }

    pub fn detach_rejection(&self, target: IVec3) -> Option<protocol::EditRejection> {
        use protocol::EditRejection;
        if self.failed {
            Some(EditRejection::PhysicsUnavailable)
        } else if !Self::valid_target(target) {
            Some(EditRejection::InvalidTarget)
        } else if self.bodies.len() >= MAX_PHYSICS_BODIES || self.next_id == u32::MAX {
            Some(EditRejection::BodyCapacity)
        } else {
            None
        }
    }

    pub fn can_detach(&self, target: IVec3) -> bool {
        !self.is_busy() && self.detach_rejection(target).is_none()
    }

    #[cfg(test)]
    pub fn detach(&mut self, target: IVec3, material: u8, direction: Vec3) {
        self.release(target, material, 0.0, direction * 5.0 + Vec3::Y * 20.0);
    }

    pub fn release(&mut self, target: IVec3, material: u8, damage: f32, impulse: Vec3) {
        self.release_at(
            target.as_vec3() + Vec3::splat(0.5),
            target,
            material,
            damage,
            impulse,
        );
    }

    /// Launch a body at a continuous position; `origin` is the restore cell
    /// used when the backend fails before the body can be regridded.
    pub fn release_at(
        &mut self,
        position: Vec3,
        origin: IVec3,
        material: u8,
        damage: f32,
        impulse: Vec3,
    ) {
        assert!(self.can_detach(origin));
        let mut body = Body::new(position.to_array(), material as u32);
        body.set_damage_joules(damage);
        self.impulses.push((self.next_id, impulse.to_array()));
        self.bodies.push(body);
        self.ids.push(self.next_id);
        self.origins.push(origin);
        self.revision += 1;
        self.next_id += 1;
        self.bodies_dirty = true;
    }

    /// Apply one reserved blast share while idle, carrying fractional fracture work
    /// and including queued momentum in the kinetic-energy calculation.
    pub fn apply_blast(
        &mut self,
        id: u32,
        load: &super::explosion::BlastLoad,
    ) -> Option<(IVec3, u8)> {
        assert!(!self.is_busy() && !self.failed);
        let slot = self.ids.iter().position(|&old| old == id)?;
        let body = &mut self.bodies[slot];
        let material = gpu_physics::material(body.material);
        body.set_damage_joules(
            body.damage_joules()
                + material.damage_energy(
                    load.contact.dissipated_energy,
                    load.contact.force,
                    load.contact.area,
                ),
        );
        self.bodies_dirty = true;
        if body.destroyed() {
            let event = (
                Vec3::from_array(body.position).floor().as_ivec3(),
                body.material as u8,
            );
            self.remove(id);
            return Some(event);
        }
        // Do not regrid a sleeping body before its reserved blast impulse runs.
        body.damage_sleep &= 0xffff;
        let pending: Vec3 = self
            .impulses
            .iter()
            .filter(|(old, _)| *old == id)
            .map(|(_, impulse)| Vec3::from_array(*impulse))
            .sum();
        let velocity = Vec3::from_array(body.velocity) + pending / material.density;
        let impulse = super::explosion::kinetic_impulse(
            material.density,
            velocity,
            load.direction,
            load.kinetic_energy,
        );
        if let Some((_, queued)) = self.impulses.iter_mut().find(|(old, _)| *old == id) {
            *queued = (Vec3::from_array(*queued) + impulse).to_array();
        } else {
            self.impulses.push((id, impulse.to_array()));
        }
        None
    }

    /// Mark edits made after submission so stale contact observations can be rejected.
    pub fn set_voxel(&mut self, target: IVec3, _material: u8) {
        if self.is_busy() || !self.contacts.is_empty() {
            self.dirty_cells.insert(target);
        }
    }


    pub fn overlaps_except(&self, target: IVec3, except: u32) -> bool {
        let center = target.as_vec3() + Vec3::splat(0.5);
        self.ids.iter().zip(&self.bodies).any(|(&id, body)| {
            id != except
                && (Vec3::from_array(body.position) - center)
                    .abs()
                    .cmplt(Vec3::splat(0.999))
                    .all()
        })
    }

    pub fn contact_overflow(&self) -> u64 {
        self.gpu.lock().terrain_contact_overflow
    }

    pub fn poll(&mut self) {
        if self.failed {
            return;
        }
        match self.gpu.get_mut().try_readback() {
            Ok(Some(bodies)) => {
                self.readback_bytes += (bodies.len() * std::mem::size_of::<Body>()
                    + gpu_physics::TERRAIN_EVENT_READBACK_BYTES)
                    as u64;
                self.bodies = bodies;
                self.contacts
                    .extend(self.gpu.get_mut().take_terrain_contacts());
                self.revision += 1;
                self.completions += 1;
                if let Some(started) = self.submitted_at.take() {
                    let us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
                    self.completion_us_total += us;
                    self.completion_us_max = self.completion_us_max.max(us);
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.failed = true;
                eprintln!(
                    "GPU physics readback failed; restoring detached blocks when source cells are clear: {error}"
                );
            }
        }
    }

    /// Ignore contact observations from a terrain version replaced during the batch.
    pub fn take_terrain_contacts(&mut self, world: &VoxelWorld) -> Vec<TerrainContact> {
        let contacts = std::mem::take(&mut self.contacts)
            .into_iter()
            .filter(|contact| {
                let target = IVec3::from_array(contact.target);
                !self.dirty_cells.contains(&target)
                    && world.block(target) == Some(contact.material as u8)
            })
            .collect();
        if !self.is_busy() {
            self.dirty_cells.clear();
        }
        contacts
    }

    pub fn destroyed_bodies(&self) -> Vec<(u32, IVec3, u8)> {
        if self.failed || self.is_busy() {
            return Vec::new();
        }
        self.ids
            .iter()
            .zip(&self.bodies)
            .filter(|(_, body)| body.destroyed())
            .map(|(&id, body)| {
                (
                    id,
                    Vec3::from_array(body.position).floor().as_ivec3(),
                    body.material as u8,
                )
            })
            .collect()
    }

    /// Latest observed body position; `None` for unknown ids.
    pub fn body_position(&self, id: u32) -> Option<Vec3> {
        self.ids
            .iter()
            .position(|&old| old == id)
            .map(|slot| Vec3::from_array(self.bodies[slot].position))
    }

    pub fn body_damage(&self, id: u32) -> f32 {
        self.ids
            .iter()
            .position(|&old| old == id)
            .map_or(0.0, |slot| self.bodies[slot].damage_joules())
    }


    pub fn failed(&self) -> bool {
        self.failed
    }
    pub fn settling_candidates(&self) -> Vec<(u32, IVec3, u8)> {
        if self.failed {
            // Keep ownership until an authoritative restore succeeds. Occupied source
            // cells are retried, never overwritten or silently discarded.
            return self
                .ids
                .iter()
                .zip(&self.origins)
                .zip(&self.bodies)
                .map(|((&id, &origin), body)| (id, origin, body.material as u8))
                .collect();
        }
        if self.gpu.lock().is_busy() {
            return Vec::new();
        }
        self.ids
            .iter()
            .zip(&self.bodies)
            .filter(|(_, body)| body.settling_candidate() && !body.destroyed())
            .filter_map(|(&id, body)| {
                let target = (Vec3::from_array(body.position) - Vec3::splat(0.5))
                    .round()
                    .as_ivec3();
                Self::valid_target(target).then_some((id, target, body.material as u8))
            })
            .collect()
    }

    /// Try nearby cells in distance order instead of getting stuck on one rounded cell.
    pub fn placement_cells(&self, id: u32, nearest: IVec3) -> Vec<IVec3> {
        if self.failed {
            return vec![nearest];
        }
        let Some(slot) = self.ids.iter().position(|&body_id| body_id == id) else {
            return Vec::new();
        };
        nearby_cells(Vec3::from_array(self.bodies[slot].position))
    }
    pub fn remove(&mut self, id: u32) {
        if let Some(slot) = self.ids.iter().position(|&old| old == id) {
            self.ids.remove(slot);
            self.bodies.remove(slot);
            self.impulses.retain(|(body_id, _)| *body_id != id);
            self.origins.remove(slot);
            self.revision += 1;
            self.bodies_dirty = true;
        }
    }

    pub fn snapshots(&self) -> Vec<PhysicsBodySnapshot> {
        self.ids
            .iter()
            .zip(&self.bodies)
            .map(|(&id, body)| PhysicsBodySnapshot {
                id,
                position: Vec3::from_array(body.position),
                velocity: Vec3::from_array(body.velocity),
                material: body.material as u8,
            })
            .collect()
    }

    pub fn player_colliders(&mut self, players: Vec<PlayerCollider>) {
        self.players = players;
    }

    pub fn dynamic_colliders(&self) -> Vec<physics::DynamicCollider> {
        self.ids
            .iter()
            .zip(&self.bodies)
            .map(|(&id, body)| {
                physics::DynamicCollider::cube(
                    id,
                    Vec3::from_array(body.position),
                    if self.failed {
                        Vec3::ZERO
                    } else {
                        Vec3::from_array(body.velocity)
                    },
                )
            })
            .collect()
    }

    pub fn step(&mut self, world: &VoxelWorld) {
        self.tick += 1;
        if self.failed || !self.tick.is_multiple_of(3) {
            return;
        }
        if self.gpu.get_mut().is_busy() {
            self.busy_ticks += 1;
            return;
        }
        let result = (|| -> Result<(), String> {
            let gpu = self.gpu.get_mut();
            let Some(bytes) = self.terrain.sync(world, &self.bodies, gpu)? else {
                self.terrain_wait_ticks += 1;
                return Ok(());
            };
            self.terrain_upload_bytes += bytes;
            self.dirty_cells.clear();
            if self.bodies_dirty {
                gpu.set_bodies(&self.bodies)?;
                self.bodies_dirty = false;
            }
            for (id, impulse) in self.impulses.drain(..) {
                if let Some(slot) = self.ids.iter().position(|&old| old == id) {
                    gpu.impulse(slot, impulse)?;
                }
            }
            if !self.bodies.is_empty() {
                gpu.set_players(&self.players)?;
                self.player_upload_bytes +=
                    (self.players.len() * std::mem::size_of::<PlayerCollider>()) as u64;
            }
            if gpu.submit(1.0 / 120.0, 6)? {
                self.submissions += 1;
                self.submitted_at = Some(std::time::Instant::now());
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.failed = true;
            eprintln!(
                "GPU physics submission failed; restoring detached blocks when source cells are clear: {error}"
            );
        }
    }
}

/// Bounded local snap: inspect at most 27 cells, within 1.5 blocks of the body.
fn nearby_cells(position: Vec3) -> Vec<IVec3> {
    let nearest = (position - Vec3::splat(0.5)).round().as_ivec3();
    let mut cells = Vec::with_capacity(27);
    for y in -1..=1 {
        for z in -1..=1 {
            for x in -1..=1 {
                let cell = nearest + IVec3::new(x, y, z);
                if PhysicsSlice::valid_target(cell)
                    && (cell.as_vec3() + Vec3::splat(0.5)).distance_squared(position) <= 2.25
                {
                    cells.push(cell);
                }
            }
        }
    }
    cells.sort_by(|a, b| {
        let distance = |cell: IVec3| (cell.as_vec3() + Vec3::splat(0.5)).distance_squared(position);
        distance(*a)
            .total_cmp(&distance(*b))
            .then_with(|| (a.y, a.z, a.x).cmp(&(b.y, b.z, b.x)))
    });
    cells
}

#[cfg(test)]
pub(super) static GPU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    fn empty_test_world() -> VoxelWorld {
        let mut world = VoxelWorld::default();
        for y in MIN_CHUNK_Y..=MAX_CHUNK_Y {
            for z in -1..=1 {
                for x in -1..=1 {
                    world.insert(
                        IVec3::new(x, y, z),
                        voxel_world::Chunk::from_runs(0, &[(32768, 0)]).unwrap(),
                    );
                }
            }
        }
        world
    }
    #[test]
    fn neighboring_snap_cells_are_local_ordered_and_allow_blocked_nearest_fallback() {
        let position = Vec3::new(0.5, 10.5, 0.5);
        let cells = nearby_cells(position);
        assert_eq!(cells[0], IVec3::new(0, 10, 0));
        // The nearest cell may be occupied: retain nearby alternatives for validation.
        assert!(
            cells
                .iter()
                .skip(1)
                .any(|cell| *cell == IVec3::new(1, 10, 0))
        );
        assert!(cells.len() <= 27);
        assert!(
            cells
                .iter()
                .all(|cell| (cell.as_vec3() + Vec3::splat(0.5)).distance(position) <= 1.5)
        );
        assert!(
            nearby_cells(Vec3::new(1000.5, 10.5, -1000.5)).contains(&IVec3::new(1000, 10, -1001))
        );
    }
    #[test]
    fn detachment_and_settlement_accept_remote_cells_within_world_bounds() {
        assert!(PhysicsSlice::valid_target(IVec3::new(1000, 20, -1000)));
        assert!(!PhysicsSlice::valid_target(IVec3::new(
            0,
            MIN_CHUNK_Y * CHUNK_SIZE,
            0
        )));
        assert!(!PhysicsSlice::valid_target(IVec3::new(i32::MIN, 20, 0)));
    }

    use super::GPU_TEST_LOCK;
    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn surface_blast_releases_multiple_blocks_and_moves_them_on_gpu() {
        use super::super::{explosion, material_damage};
        let _guard = GPU_TEST_LOCK.lock().unwrap();
        let mut world = empty_test_world();
        world.insert(
            IVec3::ZERO,
            voxel_world::Chunk::from_runs(0, &[(32768, 0)]).unwrap(),
        );
        for y in 8..=9 {
            for z in 1..=11 {
                for x in 1..=11 {
                    world.set_block(IVec3::new(x, y, z), 3).unwrap();
                }
            }
        }
        let center = Vec3::new(6.5, 10.02, 6.5);
        let mut slice = PhysicsSlice::new().unwrap();
        // Upload the intact floor first, so this also exercises terrain patches.
        slice
            .terrain
            .sync(
                &world,
                &[Body::new([6.5, 9.5, 6.5], 3)],
                slice.gpu.get_mut(),
            )
            .unwrap()
            .unwrap();
        let mut damage = std::collections::HashMap::new();
        let mut launches = Vec::new();
        for load in explosion::plan(&world, &[], &[], center, protocol::BowPower::Standard) {
            let explosion::Target::Grid(cell) = load.target else {
                unreachable!()
            };
            material_damage::apply_to_grid(&mut world, &mut damage, &load.contact, true).unwrap();
            if world.block(cell) == Some(0) {
                slice.set_voxel(cell, 0);
            } else if let Some(state) = damage.remove(&cell).filter(|state| state.release) {
                let impulse = explosion::kinetic_impulse(
                    3.0,
                    Vec3::ZERO,
                    load.direction,
                    load.kinetic_energy,
                );
                slice.release(cell, 3, state.joules, impulse);
                world.set_block(cell, 0).unwrap();
                slice.set_voxel(cell, 0);
                launches.push((
                    slice.ids[slice.ids.len() - 1],
                    cell.as_vec3() + Vec3::splat(0.5),
                    state.joules,
                    load.direction,
                ));
            }
        }
        assert!(launches.len() >= 8);
        assert_eq!(world.block(IVec3::new(6, 9, 6)), Some(0));
        assert_eq!(world.block(IVec3::new(6, 8, 6)), Some(3));
        for _ in 0..15 {
            slice.step(&world);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while slice.is_busy() {
                slice.poll();
                assert!(
                    std::time::Instant::now() < deadline,
                    "GPU completion timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        assert!(!slice.failed());
        let mut moved = 0;
        let mut lifted = 0;
        for (id, start, damage, direction) in &launches {
            let slot = slice.ids.iter().position(|body_id| body_id == id).unwrap();
            let body = &slice.bodies[slot];
            assert!(body.damage_joules() >= *damage);
            let displacement = Vec3::from_array(body.position) - *start;
            if !body.destroyed() && displacement.y > 0.5 {
                lifted += 1;
            }
            if !body.destroyed() && displacement.dot(*direction) > 0.1 {
                moved += 1;
            }
        }
        eprintln!(
            "surface blast GPU: {} released, {moved} moved, {lifted} lifted >0.5m",
            launches.len()
        );
        assert!(
            moved >= 8,
            "surviving blocks must actually move after detachment"
        );
        assert!(
            lifted >= 4,
            "the blast should visibly eject multiple surviving blocks"
        );
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn blast_wakes_settlement_candidate_and_submits_reserved_energy_once() {
        let _guard = GPU_TEST_LOCK.lock().unwrap();
        let mut world = empty_test_world();
        world.insert(
            IVec3::ZERO,
            voxel_world::Chunk::from_runs(0, &[(32768, 0)]).unwrap(),
        );
        let mut slice = PhysicsSlice::new().unwrap();
        slice.release(IVec3::new(4, 20, 4), 3, 6.0, Vec3::ZERO);
        let id = slice.ids[0];
        slice.bodies[0].damage_sleep |= 100 << 16;
        assert_eq!(slice.settling_candidates().len(), 1);
        let load = super::super::explosion::BlastLoad {
            target: super::super::explosion::Target::Body(id),
            contact: TerrainContact {
                target: [0; 3],
                material: 3,
                dissipated_energy: 4.0,
                force: 5000.0,
                area: 1.0,
            },
            direction: Vec3::X,
            kinetic_energy: 12.0,
            player_damage: 0,
        };
        assert_eq!(slice.apply_blast(id, &load), None);
        assert_eq!(slice.apply_blast(id, &load), None);
        assert!(slice.settling_candidates().is_empty());
        assert_eq!(slice.body_damage(id), 10.0);
        assert_eq!(slice.impulses.len(), 1);
        let momentum = Vec3::from_array(slice.impulses[0].1);
        assert!((momentum.length_squared() / (2.0 * 3.0) - 24.0).abs() < 0.001);
        // Two wall ticks before the upload must not offer this body for regridding.
        for _ in 0..2 {
            slice.step(&world);
            assert!(slice.settling_candidates().is_empty());
            assert_eq!(slice.impulses.len(), 1);
        }
        slice.step(&world);
        assert!(slice.impulses.is_empty());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while slice.is_busy() {
            slice.poll();
            assert!(
                std::time::Instant::now() < deadline,
                "GPU completion timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(!slice.failed());
        assert_eq!(slice.completions, 1);
        assert_eq!(slice.body_damage(id), 10.0);
        assert!((slice.bodies[0].velocity[0] - 4.0).abs() < 0.01);
        assert!(slice.bodies[0].position[0] > 4.6);
        assert!(slice.settling_candidates().is_empty());

        let destructive = super::super::explosion::BlastLoad {
            contact: TerrainContact {
                dissipated_energy: 200.0,
                ..load.contact
            },
            ..load
        };
        assert!(slice.apply_blast(id, &destructive).is_some());
        assert!(slice.snapshots().is_empty());
        assert!(slice.apply_blast(id, &destructive).is_none());
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn lifecycle_uses_sparse_uploads_and_retains_failed_bodies_for_restore() {
        let _guard = GPU_TEST_LOCK.lock().unwrap();
        let mut world = VoxelWorld::default();
        for y in -1..=1 {
            for z in -1..=0 {
                for x in -1..=0 {
                    world.ensure_chunk(IVec3::new(x, y, z));
                }
            }
        }
        let target = (0..32)
            .rev()
            .map(|y| IVec3::new(0, y, 0))
            .find(|&p| world.block(p).is_some_and(|b| b != 0))
            .unwrap();
        let material = world.block(target).unwrap();
        let mut slice = PhysicsSlice::new().unwrap();
        for _ in 0..3 {
            slice.step(&world);
        }
        slice.release(target, material, 6.0, Vec3::Y * 5.0);
        let id = slice.ids[0];
        world.set_block(target, 0).unwrap();
        slice.set_voxel(target, 0);
        let mut settled = false;
        for _ in 0..600 {
            slice.step(&world);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while slice.gpu.get_mut().is_busy() {
                slice.poll();
                assert!(
                    std::time::Instant::now() < deadline,
                    "GPU completion timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if !slice.settling_candidates().is_empty() {
                settled = true;
                break;
            }
        }
        assert!(
            settled,
            "detached block should land and become a grid candidate"
        );
        assert!(slice.terrain_upload_bytes > 0 && slice.resident_chunks() > 0);
        assert!(slice.completions > 0);
        assert!(
            slice.body_damage(id) >= 6.0,
            "damage must survive GPU ownership and settlement"
        );
        let revision = slice.revision;
        slice.poll();
        assert_eq!(
            slice.revision, revision,
            "idle polling is not a new physics state"
        );
        // Inject a backend fault after the last completed observation. Ownership is
        // retained and the source cell proposed for authoritative restoration.
        slice.failed = true;
        assert_eq!(slice.settling_candidates(), vec![(id, target, material)]);
        assert!(!slice.can_detach(target));
        assert_eq!(slice.snapshots().len(), 1);
        slice.remove(id);
        assert!(slice.settling_candidates().is_empty());
        assert!(slice.origins.is_empty());
    }

    #[test]
    #[ignore = "requires a headless GPU adapter"]
    fn blocked_character_pushes_gpu_body_and_can_follow_it() {
        let _guard = GPU_TEST_LOCK.lock().unwrap();
        let mut world = VoxelWorld::default();
        for y in -1..=1 {
            for z in -1..=0 {
                for x in -1..=0 {
                    world.insert(
                        IVec3::new(x, y, z),
                        voxel_world::Chunk::from_runs(0, &[(32768, if y < 0 { 3 } else { 0 })])
                            .unwrap(),
                    );
                }
            }
        }
        let target = IVec3::new(1, 0, 0);
        world.set_block(target, 3).unwrap();
        let mut slice = PhysicsSlice::new().unwrap();
        slice.detach(target, 3, Vec3::ZERO);
        slice.impulses.clear(); // Start resting; only walking should move this body.
        world.set_block(target, 0).unwrap();
        slice.set_voxel(target, 0);
        let mut player = physics::PlayerState {
            position: Vec3::new(0.5, 0.0, 0.5),
            velocity: Vec3::ZERO,
            grounded: true,
            ..Default::default()
        };
        let input = controller::PlayerInput {
            movement: [1.0, 0.0],
            ..Default::default()
        };
        for _ in 0..60 {
            let mut intended = player;
            controller::step_player(&world, &mut intended, &input, physics::FIXED_DT);
            controller::step_player_with_bodies(
                &world,
                &mut player,
                &input,
                physics::FIXED_DT,
                &slice.dynamic_colliders(),
            );
            slice.player_colliders(vec![PlayerCollider {
                position: player.position.to_array(),
                id: 0,
                velocity: [intended.velocity.x, player.velocity.y, intended.velocity.z],
                padding: 0,
            }]);
            slice.step(&world);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while slice.gpu.get_mut().is_busy() {
                slice.poll();
                assert!(
                    std::time::Instant::now() < deadline,
                    "GPU completion timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        assert!(
            slice.bodies[0].position[0] > 2.0,
            "walking should push body: {:?}",
            slice.bodies[0]
        );
        assert!(
            player.position.x > 1.0,
            "character should follow displaced body: {player:?}"
        );
        assert!(player.position.y >= -0.001);
        assert_eq!(slice.player_upload_bytes, slice.submissions * 32);
    }
}
