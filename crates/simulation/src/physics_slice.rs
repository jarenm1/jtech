//! Server-side lifecycle around the bounded GPU backend. CPU copies are observations,
//! never re-uploaded between lifecycle edits. Six 120 Hz substeps share one readback.
use glam::{IVec3, Vec3};
use gpu_physics::{Body, GpuPhysics, PlayerCollider, Terrain, TerrainContact};
use protocol::{MAX_PHYSICS_BODIES, PhysicsBodySnapshot};
use voxel_world::VoxelWorld;

const ORIGIN: IVec3 = IVec3::new(-16, -8, -16);
const SIZE: IVec3 = IVec3::new(32, 48, 32);

pub(super) struct PhysicsSlice {
    // wgpu map completion uses an mpsc Receiver, so protect it for Bevy's Sync bound.
    gpu: parking_lot::Mutex<GpuPhysics>,
    terrain: Terrain,
    bodies: Vec<Body>,
    ids: Vec<u32>,
    origins: Vec<IVec3>,
    next_id: u32,
    impulses: Vec<(u32, [f32; 3])>,
    players: Vec<PlayerCollider>,
    bodies_dirty: bool,
    terrain_dirty: bool,
    dirty_cells: std::collections::BTreeSet<usize>,
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
}

impl PhysicsSlice {
    pub fn new() -> Result<Self, String> {
        let terrain = Terrain {
            origin: ORIGIN.to_array(),
            size: SIZE.as_uvec3().to_array(),
            cells: vec![0; (SIZE.x * SIZE.y * SIZE.z) as usize],
        };
        let gpu = GpuPhysics::new(terrain.clone())?;
        eprintln!(
            "GPU physics: {} region={:?} size={:?} body_cap={} step_hz=120 readback_hz=20",
            gpu.adapter_name, ORIGIN, SIZE, MAX_PHYSICS_BODIES
        );
        Ok(Self {
            gpu: parking_lot::Mutex::new(gpu),
            terrain,
            bodies: Vec::new(),
            ids: Vec::new(),
            origins: Vec::new(),
            next_id: 1,
            impulses: Vec::new(),
            players: Vec::new(),
            bodies_dirty: false,
            terrain_dirty: false,
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
        })
    }

    pub fn initialize_terrain(&mut self, world: &VoxelWorld) {
        for y in 0..SIZE.y {
            for z in 0..SIZE.z {
                for x in 0..SIZE.x {
                    let local = IVec3::new(x, y, z);
                    let i = self.terrain.index(x as u32, y as u32, z as u32);
                    self.terrain.cells[i] = world.block(ORIGIN + local).unwrap_or(3) as u32;
                }
            }
        }
        self.terrain_dirty = true;
    }

    fn in_region(target: IVec3) -> bool {
        target.cmpge(ORIGIN + IVec3::ONE).all() && target.cmplt(ORIGIN + SIZE - IVec3::ONE).all()
    }

    pub fn is_busy(&self) -> bool {
        self.gpu.lock().is_busy()
    }

    pub fn detach_rejection(&self, target: IVec3) -> Option<protocol::EditRejection> {
        use protocol::EditRejection;
        if self.failed {
            Some(EditRejection::PhysicsUnavailable)
        } else if !Self::in_region(target) {
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
        assert!(self.can_detach(target));
        let mut body = Body::new(
            (target.as_vec3() + Vec3::splat(0.5)).to_array(),
            material as u32,
        );
        body.set_damage_joules(damage);
        self.impulses.push((self.next_id, impulse.to_array()));
        self.bodies.push(body);
        self.ids.push(self.next_id);
        self.origins.push(target);
        self.revision += 1;
        self.next_id += 1;
        self.bodies_dirty = true;
    }

    pub fn set_voxel(&mut self, target: IVec3, material: u8) {
        let p = target - ORIGIN;
        if p.cmpge(IVec3::ZERO).all() && p.cmplt(SIZE).all() {
            let i = self.terrain.index(p.x as u32, p.y as u32, p.z as u32);
            if self.terrain.cells[i] != material as u32 {
                self.terrain.cells[i] = material as u32;
                self.dirty_cells.insert(i);
            }
        }
    }

    pub fn overlaps(&self, target: IVec3) -> bool {
        self.overlaps_except(target, 0)
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
    pub fn take_terrain_contacts(&mut self) -> Vec<TerrainContact> {
        std::mem::take(&mut self.contacts)
            .into_iter()
            .filter(|contact| {
                let p = IVec3::from_array(contact.target) - ORIGIN;
                if p.cmplt(IVec3::ZERO).any() || p.cmpge(SIZE).any() {
                    return false;
                }
                let i = self.terrain.index(p.x as u32, p.y as u32, p.z as u32);
                !self.dirty_cells.contains(&i) && self.terrain.cells[i] == contact.material
            })
            .collect()
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

    pub fn body_damage(&self, id: u32) -> f32 {
        self.ids
            .iter()
            .position(|&old| old == id)
            .map_or(0.0, |slot| self.bodies[slot].damage_joules())
    }

    /// A placement cannot use stale CPU observations while a GPU batch is in flight.
    pub fn can_place(&self, target: IVec3) -> bool {
        let inside = target.cmpge(ORIGIN).all() && target.cmplt(ORIGIN + SIZE).all();
        !(self.overlaps(target) || inside && self.gpu.lock().is_busy())
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
                (target.cmpge(ORIGIN).all() && target.cmplt(ORIGIN + SIZE).all()).then_some((
                    id,
                    target,
                    body.material as u8,
                ))
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
            .map(|(&id, body)| physics::DynamicCollider {
                id,
                position: Vec3::from_array(body.position),
                velocity: if self.failed {
                    Vec3::ZERO
                } else {
                    Vec3::from_array(body.velocity)
                },
            })
            .collect()
    }

    pub fn step(&mut self) {
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
            if self.terrain_dirty {
                gpu.update_terrain(&self.terrain.cells)?;
                self.terrain_upload_bytes += (self.terrain.cells.len() * 4) as u64;
                self.terrain_dirty = false;
            } else {
                // Coalesce adjacent changes: one 4-byte cell upload for a lone edit.
                let mut dirty = self.dirty_cells.iter().copied().peekable();
                while let Some(start) = dirty.next() {
                    let mut end = start + 1;
                    while dirty.peek() == Some(&end) {
                        dirty.next();
                        end += 1;
                    }
                    gpu.update_terrain_range(start, &self.terrain.cells[start..end])?;
                    self.terrain_upload_bytes += ((end - start) * 4) as u64;
                }
            }
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
                if cell.cmpge(ORIGIN).all()
                    && cell.cmplt(ORIGIN + SIZE).all()
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
mod tests {
    use super::*;
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
            nearby_cells(ORIGIN.as_vec3())
                .iter()
                .all(|cell| cell.cmpge(ORIGIN).all())
        );
    }
    #[test]
    fn region_bounds_leave_a_solid_margin() {
        assert!(PhysicsSlice::in_region(IVec3::new(0, 20, 0)));
        assert!(!PhysicsSlice::in_region(ORIGIN));
        assert!(!PhysicsSlice::in_region(ORIGIN + SIZE));
        assert!(!PhysicsSlice::in_region(IVec3::new(1000, 20, 0)));
    }

    static GPU_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
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
        slice.initialize_terrain(&world);
        for _ in 0..3 {
            slice.step();
        }
        let initial_bytes = slice.terrain_upload_bytes;
        slice.release(target, material, 6.0, Vec3::Y * 5.0);
        let id = slice.ids[0];
        world.set_block(target, 0).unwrap();
        slice.set_voxel(target, 0);
        let mut settled = false;
        for _ in 0..600 {
            slice.step();
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
        assert_eq!(slice.terrain_upload_bytes - initial_bytes, 4);
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
        slice.initialize_terrain(&world);
        slice.detach(target, 3, Vec3::ZERO);
        slice.impulses.clear(); // Start resting; only walking should move this body.
        world.set_block(target, 0).unwrap();
        slice.set_voxel(target, 0);
        let mut player = physics::PlayerState {
            position: Vec3::new(0.5, 0.0, 0.5),
            velocity: Vec3::ZERO,
            grounded: true,
        };
        let input = physics::PlayerInput {
            movement: [1.0, 0.0],
            ..Default::default()
        };
        for _ in 0..60 {
            let mut intended = player;
            physics::step_player(&world, &mut intended, &input, physics::FIXED_DT);
            physics::step_player_with_bodies(
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
            slice.step();
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
