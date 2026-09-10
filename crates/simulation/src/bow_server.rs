//! Bow request admission, projectile replication, and authoritative blast transactions.
use super::{
    Simulation,
    bow::{Arrow, Flight, SHOT_COOLDOWN_TICKS},
    explosion::{self, Target},
};
use glam::Vec3;
use physics::{EYE_HEIGHT, look_direction};
use protocol::{EditRejection, MAX_ARROWS, ServerMessage};
use voxel_world::VoxelWorld;

impl Simulation {
    pub(super) fn fire_bow(
        &mut self,
        world: &VoxelWorld,
        id: u64,
        request: u64,
        yaw: f32,
        pitch: f32,
    ) {
        let Some(player) = self.players.get(&id) else {
            return;
        };
        if let Some((_, outcome)) = player.results.iter().find(|(old, _)| *old == request) {
            self.send_edit_result(id, request, *outcome);
            return;
        }
        if self
            .strikes
            .iter()
            .any(|s| s.id == id && s.request == request)
        {
            return;
        }
        let origin = player.state.position + Vec3::Y * EYE_HEIGHT;
        let rejection = if request <= player.highest_request {
            Some(EditRejection::OldRequest)
        } else if self.tick < player.next_bow_tick {
            Some(EditRejection::Cooldown)
        } else if !yaw.is_finite()
            || !pitch.is_finite()
            || pitch.abs() > std::f32::consts::FRAC_PI_2
        {
            Some(EditRejection::InvalidTarget)
        } else if world.block(origin.floor().as_ivec3()) != Some(0) {
            Some(EditRejection::Occupied)
        } else if self.arrows.len() + self.detonations.len() >= MAX_ARROWS
            || self.next_arrow == u32::MAX
        {
            Some(EditRejection::BodyCapacity)
        } else if self.physics.as_ref().is_some_and(|p| p.failed()) {
            Some(EditRejection::PhysicsUnavailable)
        } else {
            None
        };
        if let Some(reason) = rejection {
            self.finish_edit(id, request, Err(reason));
            return;
        }
        self.arrows.push(Arrow::new(
            self.next_arrow,
            origin,
            look_direction(yaw, pitch),
        ));
        self.next_arrow += 1;
        self.arrow_revision += 1;
        self.metrics.bow_shots += 1;
        self.players.get_mut(&id).unwrap().next_bow_tick =
            self.tick.saturating_add(SHOT_COOLDOWN_TICKS);
        self.finish_edit(id, request, Ok(None));
    }

    pub(super) fn advance_bow(&mut self, world: &mut VoxelWorld) {
        let bodies = self
            .physics
            .as_ref()
            .filter(|p| !p.failed())
            .map(|p| p.snapshots())
            .unwrap_or_default();
        let ready = self
            .physics
            .as_ref()
            .is_none_or(|p| !p.is_busy() || p.failed());
        let arrows = std::mem::take(&mut self.arrows);
        if !arrows.is_empty() {
            self.arrow_revision += 1;
        }
        for mut arrow in arrows {
            match arrow.tick(world, &bodies, ready) {
                Flight::Flying => self.arrows.push(arrow),
                Flight::Impact(position) => {
                    self.detonations.push_back((arrow.snapshot.id, position))
                }
                Flight::Expired => {}
            }
        }
        self.detonate_ready(world);
        if self.tick.is_multiple_of(3) {
            let recipients: Vec<_> = self
                .players
                .iter()
                .filter(|(_, player)| player.arrow_revision != Some(self.arrow_revision))
                .map(|(&id, _)| id)
                .collect();
            let message = ServerMessage::Projectiles {
                tick: self.arrow_revision,
                arrows: self.arrows.iter().map(|arrow| arrow.snapshot).collect(),
            };
            for id in recipients {
                if self.send(id, &message) {
                    self.players.get_mut(&id).unwrap().arrow_revision = Some(self.arrow_revision);
                }
            }
        }
    }

    fn detonate_ready(&mut self, world: &mut VoxelWorld) {
        // Never mutate or overwrite GPU-owned bodies using an in-flight observation.
        if self
            .physics
            .as_ref()
            .is_some_and(|p| p.is_busy() && !p.failed())
        {
            return;
        }
        let mut physics = self.physics.take();
        // Bound voxel scans, transactions, and replication work per server tick.
        for _ in 0..2 {
            let Some((id, position)) = self.detonations.pop_front() else {
                break;
            };
            let bodies = physics
                .as_ref()
                .filter(|p| !p.failed())
                .map(|p| p.snapshots())
                .unwrap_or_default();
            let loads = explosion::plan(world, &bodies, position);
            for load in loads {
                match load.target {
                    Target::Grid(target) => {
                        if self.apply_contact(world, &load.contact).is_err() {
                            self.metrics.rejected_contacts += 1;
                            continue;
                        }
                        if let Some(physics) = &mut physics {
                            if world.block(target) == Some(0) {
                                physics.set_voxel(target, 0);
                                continue;
                            }
                            // An intact attachment absorbs the reaction in the grid.
                            // Only a successful release may consume reserved motion energy.
                            if let Some(state) = self.damage.get(&target).copied()
                                && state.release
                                && physics.can_detach(target)
                                && self.has_journal_space(target)
                                && let Some((from, to)) = world.set_block(target, 0)
                            {
                                let mass = gpu_physics::material(state.material as u32).density;
                                let impulse = explosion::kinetic_impulse(
                                    mass,
                                    Vec3::ZERO,
                                    load.direction,
                                    load.kinetic_energy,
                                );
                                physics.release(target, state.material, state.joules, impulse);
                                physics.set_voxel(target, 0);
                                self.record_change(world, target, 0, from, to);
                            }
                        }
                    }
                    Target::Body(body_id) => {
                        if let Some(physics) = &mut physics
                            && let Some((target, material)) = physics.apply_blast(body_id, &load)
                        {
                            self.destroyed(target, material, Some(body_id));
                        }
                    }
                }
            }
            self.metrics.explosions += 1;
            let event = ServerMessage::Explosion {
                id,
                position,
                radius: explosion::RADIUS,
            };
            let recipients: Vec<_> = self.players.keys().copied().collect();
            for recipient in recipients {
                self.send(recipient, &event);
            }
        }
        self.physics = physics;
    }
}
