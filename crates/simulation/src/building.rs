//! Authoritative free-placed building pieces. Placement and melee hits arrive
//! as requests over the same `finish_edit` result channel as voxel edits; the
//! server re-raycasts the aim so a client can only name a kind and a target,
//! never a position. Pieces are replicated whole on every revision change.
use super::Simulation;
use gameplay::building::{self, Piece, PieceKind, PlacementError};
use glam::Vec3;
use physics::{DynamicCollider, EYE_HEIGHT, look_direction};
use protocol::{EditRejection, PieceSnapshot, ServerMessage};
use voxel_world::VoxelWorld;

/// `PlacementError` onto the wire rejection vocabulary: geometry and bounds
/// faults are bad targets, occupancy faults are contention.
fn placement_rejection(error: PlacementError) -> EditRejection {
    match error {
        PlacementError::Unloaded | PlacementError::Terrain | PlacementError::OutOfBounds => {
            EditRejection::InvalidTarget
        }
        PlacementError::Piece | PlacementError::Player => EditRejection::Occupied,
    }
}

impl Simulation {
    /// Place one building primitive against the surface `id` is aiming at.
    /// Ordering mirrors `validate_player_edit`: dead, replayed, and cooling
    /// requests reject before stock, capacity, and geometry are consulted.
    pub(super) fn place_piece(
        &mut self,
        world: &VoxelWorld,
        id: u64,
        request: u64,
        kind: PieceKind,
        yaw_steps: u8,
    ) {
        let Some(player) = self.players.get(&id) else {
            return;
        };
        if let Some((_, outcome)) = player.results.iter().find(|(old, _)| *old == request) {
            self.send_edit_result(id, request, *outcome);
            return;
        }
        if player.health.is_depleted() {
            self.finish_edit(id, request, Err(EditRejection::Dead));
            return;
        }
        if request <= player.highest_request {
            self.finish_edit(id, request, Err(EditRejection::OldRequest));
            return;
        }
        if self.tick < player.last_edit.saturating_add(6) {
            self.finish_edit(id, request, Err(EditRejection::Cooldown));
            return;
        }
        if player.inventory.count(kind.item()) == 0 {
            self.finish_edit(id, request, Err(EditRejection::OutOfStock));
            return;
        }
        if self.pieces.len() >= building::MAX_PIECES {
            self.finish_edit(id, request, Err(EditRejection::BodyCapacity));
            return;
        }
        // Re-raycast the aim: the client names only a kind, never a position.
        let eye = player.state.position + Vec3::Y * EYE_HEIGHT;
        let direction = look_direction(player.input.yaw, player.input.pitch);
        let Some(hit) = world.raycast(eye, direction, building::BUILD_REACH) else {
            self.finish_edit(id, request, Err(EditRejection::OutOfReach));
            return;
        };
        let hit_point = eye + direction * hit.distance;
        let normal = building::hit_normal(&hit);
        let Some(position) = building::placement_pose(kind, yaw_steps, hit_point, normal) else {
            self.finish_edit(id, request, Err(EditRejection::InvalidTarget));
            return;
        };
        if let Err(error) = building::placement_clear(
            world,
            kind,
            yaw_steps,
            position,
            self.pieces.values(),
            self.players.values().map(|player| &player.state),
        ) {
            self.finish_edit(id, request, Err(placement_rejection(error)));
            return;
        }
        // Loose bodies are observed, not simulated here: while a GPU batch is
        // in flight their positions are stale, so placement waits rather than
        // intersecting a body it cannot see.
        if self.physics.as_ref().is_some_and(|p| p.is_busy()) {
            self.finish_edit(id, request, Err(EditRejection::PhysicsUnavailable));
            return;
        }
        let (min, max) = (position - kind.half_extents(yaw_steps), position + kind.half_extents(yaw_steps));
        let occupied = self
            .physics
            .as_ref()
            .map(|p| p.dynamic_colliders())
            .unwrap_or_default()
            .iter()
            .any(|body| {
                let (lo, hi) = body.aabb();
                building::aabb_overlap(min, max, lo, hi)
            });
        if occupied {
            self.finish_edit(id, request, Err(EditRejection::Occupied));
            return;
        }
        let piece_id = self.next_piece;
        self.next_piece = self.next_piece.wrapping_add(1).max(1);
        self.pieces.insert(
            piece_id,
            Piece {
                id: piece_id,
                kind,
                position,
                yaw_steps,
                health: kind.health(),
            },
        );
        let player = self.players.get_mut(&id).unwrap();
        player.inventory.take(kind.item(), 1);
        player.inventory_dirty = true;
        // Placing a bedroll binds the owner's respawn to the piece.
        if kind == PieceKind::Bedroll {
            player.respawn_point = Some(piece_id);
        }
        self.piece_revision += 1;
        self.finish_edit(id, request, Ok(None));
    }

    /// One melee strike against a placed piece. The attack cadence already
    /// gates swings, so only the dead/replayed checks precede the re-raycast.
    pub(super) fn hit_piece(&mut self, world: &VoxelWorld, id: u64, request: u64, piece_id: u32) {
        let Some(player) = self.players.get(&id) else {
            return;
        };
        if let Some((_, outcome)) = player.results.iter().find(|(old, _)| *old == request) {
            self.send_edit_result(id, request, *outcome);
            return;
        }
        if player.health.is_depleted() {
            self.finish_edit(id, request, Err(EditRejection::Dead));
            return;
        }
        if request <= player.highest_request {
            self.finish_edit(id, request, Err(EditRejection::OldRequest));
            return;
        }
        let Some(piece) = self.pieces.get(&piece_id) else {
            self.finish_edit(id, request, Err(EditRejection::InvalidTarget));
            return;
        };
        let spec = gameplay::combat::melee_spec(gameplay::combat::slot_item(
            player.input.selected,
        ));
        let eye = player.state.position + Vec3::Y * EYE_HEIGHT;
        let direction = look_direction(player.input.yaw, player.input.pitch);
        // The ray must enter this piece within the held tool's reach, and no
        // terrain may occlude it first.
        let in_reach = building::raycast_piece(eye, direction, piece)
            .is_some_and(|distance| {
                distance <= spec.range && world.raycast(eye, direction, distance).is_none()
            });
        if !in_reach {
            self.finish_edit(id, request, Err(EditRejection::OutOfReach));
            return;
        }
        let damage = spec.damage;
        let piece = self.pieces.get_mut(&piece_id).unwrap();
        piece.health = piece.health.saturating_sub(damage);
        if piece.health == 0 {
            let piece = self.pieces.remove(&piece_id).unwrap();
            self.spawn_drop(piece.position, piece.kind.item(), 1);
            self.piece_revision += 1;
            // Destroying a bedroll unbinds every respawn anchored to it.
            for player in self.players.values_mut() {
                if player.respawn_point == Some(piece_id) {
                    player.respawn_point = None;
                }
            }
        }
        self.finish_edit(id, request, Ok(Some(f32::from(damage))));
    }

    /// Static colliders for the character motor and spawn clearance; positions
    /// are box centers like the loose-body set they extend.
    pub(super) fn piece_colliders(&self) -> Vec<DynamicCollider> {
        self.pieces
            .values()
            .map(|piece| DynamicCollider {
                id: piece.id,
                position: piece.position,
                velocity: Vec3::ZERO,
                half_extents: piece.half_extents(),
            })
            .collect()
    }

    /// Reliably resend the whole bounded set to players behind the revision.
    /// New joins carry `None`, so their first send is the full snapshot.
    pub(super) fn replicate_pieces(&mut self) {
        let stale: Vec<_> = self
            .players
            .iter()
            .filter(|(_, player)| player.piece_revision != Some(self.piece_revision))
            .map(|(&id, _)| id)
            .collect();
        if stale.is_empty() {
            return;
        }
        let pieces: Vec<PieceSnapshot> = self
            .pieces
            .values()
            .map(|piece| PieceSnapshot {
                id: piece.id,
                kind: piece.kind,
                position: piece.position,
                yaw_steps: piece.yaw_steps,
                health: piece.health,
            })
            .collect();
        for id in stale {
            if self.send(
                id,
                &ServerMessage::Building {
                    revision: self.piece_revision,
                    pieces: pieces.clone(),
                },
            ) {
                self.players.get_mut(&id).unwrap().piece_revision = Some(self.piece_revision);
            }
        }
    }
}
