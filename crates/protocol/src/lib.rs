mod bow_power;
pub use bow_power::BowPower;
use controller::PlayerInput;
pub use gameplay::{Health, Inventory, building::PieceKind};
use glam::{IVec3, Vec3};
use physics::PlayerState;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io;

pub const PROTOCOL_VERSION: u32 = 17;
pub const MAX_PHYSICS_BODIES: usize = 128;
pub const MAX_PLAYERS: usize = 16;
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_FRAME: usize = 128 * 1024;
pub const MAX_ARROWS: usize = 32;
/// Ceiling for one complete dropped-item replication, matching the server bound.
pub const MAX_DROPS: usize = 64;
pub const EXPLOSIVE_BOW_SLOT: u8 = 6;
pub const EXPLOSIVE_BOW_SHOTS_PER_SECOND: u32 = 25;
/// Horizontal chunk radius shared by server configuration and client camera bounds.
pub const DEFAULT_VIEW_RADIUS: i32 = 16;
pub const MAX_VIEW_RADIUS: i32 = 64;
/// Ceiling for the placed building-piece set, matching the server bound.
pub const MAX_PIECES: usize = gameplay::building::MAX_PIECES;

/// Authoritative action rejection, also used for completed queued debug strikes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum EditRejection {
    OldRequest,
    Cooldown,
    InvalidTarget,
    StaleRevision,
    OutOfReach,
    Occupied,
    PhysicsUnavailable,
    BodyCapacity,
    QueueFull,
    Expired,
    StorageFull,
    RevisionExhausted,
    PackageUnavailable,
    Dead,
    OutOfStock,
}

/// One placed building primitive. `position` is the box center; `yaw_steps`
/// counts quarter turns about Y. Free-standing AABBs, never voxel cells.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct PieceSnapshot {
    pub id: u32,
    pub kind: PieceKind,
    pub position: Vec3,
    pub yaw_steps: u8,
    pub health: u16,
}

/// One voxel's replicated state. Material bytes in chunk runs carry the
/// placed flag in the high bit; deltas send it as a plain field instead.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Voxel {
    pub material: u8,
    pub density: i8,
    pub placed: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello {
        version: u32,
    },
    Edit {
        request: u64,
        target: IVec3,
        block: u8,
        expected_revision: u64,
    },
    /// Place a building piece at the surface the player is aiming at. The
    /// server re-raycasts the aim and validates clearance; `yaw_steps` is the
    /// client's chosen quarter-turn rotation.
    Place {
        request: u64,
        kind: PieceKind,
        yaw_steps: u8,
    },
    /// Melee strike against a placed piece. The server re-raycasts against the
    /// piece set and applies the held tool's damage.
    HitPiece {
        request: u64,
        piece: u32,
    },
    Resync {
        coord: IVec3,
    },
    Strike {
        request: u64,
        target: IVec3,
        expected_revision: u64,
    },
    /// Aim and power preset; the server authors muzzle, velocity, blast, and cooldown.
    FireBow {
        request: u64,
        yaw: f32,
        pitch: f32,
        power: BowPower,
    },
    Respawn {
        life: u64,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome {
        id: u64,
        session: u64,
        seed: u64,
        spawn: PlayerState,
        health: Health,
        inventory: Inventory,
    },
    Chunk {
        coord: IVec3,
        revision: u64,
        /// Material runs with the placed flag packed into the high bit.
        material_runs: Vec<(u16, u8)>,
        density_runs: Vec<(u16, i8)>,
    },
    Delta {
        coord: IVec3,
        from: u64,
        to: u64,
        /// `(local index, post-edit voxel)` pairs; `to - from` equals the count.
        voxels: Vec<(u16, Voxel)>,
    },
    Forget {
        coord: IVec3,
    },
    EditResult {
        request: u64,
        accepted: bool,
        reason: Option<EditRejection>,
        /// Accumulated fracture damage / destruction budget for a successful hit.
        damage: Option<f32>,
    },
    Disconnect {
        reason: String,
    },
    /// Complete replacement of the bounded dynamic-body set; positions are centers.
    Physics {
        tick: u64,
        bodies: Vec<PhysicsBodySnapshot>,
    },
    /// Complete replacement of the bounded projectile set, at 20 Hz.
    Projectiles {
        tick: u64,
        arrows: Vec<ArrowSnapshot>,
    },
    /// Authoritative detonation. Clients use this only for presentation.
    Explosion {
        id: u32,
        position: Vec3,
        radius: f32,
    },
    /// Server package lifecycle and authoritative weapon cadence, sent reliably.
    Packages {
        revision: u64,
        packages: Vec<PackageStatus>,
        bow_shots_per_second: u32,
    },
    /// Authoritative owned-item counts for the receiving player, sent reliably on change.
    Inventory {
        inventory: Inventory,
    },
    /// Complete replacement of the bounded dropped-item set, at 20 Hz.
    Drops {
        tick: u64,
        drops: Vec<DropSnapshot>,
    },
    /// Complete replacement of the bounded building-piece set, sent reliably
    /// on every change.
    Building {
        revision: u64,
        pieces: Vec<PieceSnapshot>,
    },
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PackageState {
    Loading,
    Reloading,
    Loaded,
    Error,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageStatus {
    pub id: String,
    /// Last successfully loaded generation; zero means no usable package.
    pub generation: u64,
    pub state: PackageState,
    pub error: Option<String>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PhysicsBodySnapshot {
    pub id: u32,
    pub position: Vec3,
    pub velocity: Vec3,
    pub material: u8,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct ArrowSnapshot {
    pub id: u32,
    pub position: Vec3,
    pub velocity: Vec3,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DropSnapshot {
    pub id: u32,
    pub item: u8,
    pub count: u16,
    pub position: Vec3,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub id: u64,
    pub last_input: u64,
    pub state: PlayerState,
    pub health: Health,
    pub life: u64,
    pub yaw: f32,
}
/// Non-player character state at 20 Hz. `id` is stable across respawns.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActorSnapshot {
    pub id: u32,
    pub state: PlayerState,
    pub health: Health,
    pub yaw: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub tick: u64,
    pub you: PlayerSnapshot,
    pub players: Vec<PlayerSnapshot>,
    pub actors: Vec<ActorSnapshot>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputPacket {
    pub session: u64,
    pub life: u64,
    pub inputs: Vec<PlayerInput>,
}

pub fn encode<T: Serialize>(value: &T, limit: usize) -> io::Result<Vec<u8>> {
    let bytes = postcard::to_stdvec(value).map_err(invalid)?;
    if bytes.len() > limit {
        return Err(invalid("encoded message exceeds limit"));
    }
    Ok(bytes)
}
pub fn decode<T: DeserializeOwned>(bytes: &[u8], limit: usize) -> io::Result<T> {
    if bytes.len() > limit {
        return Err(invalid("message exceeds limit"));
    }
    let (value, rest) = postcard::take_from_bytes(bytes).map_err(invalid)?;
    if !rest.is_empty() {
        return Err(invalid("trailing bytes"));
    }
    Ok(value)
}
fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gameplay::MAX_STACK;

    #[test]
    fn player_health_round_trips_in_welcome_and_snapshot() {
        let mut health = Health::default();
        health.damage(37);
        let welcome = ServerMessage::Welcome {
            id: 1,
            session: 2,
            seed: 7,
            spawn: PlayerState::default(),
            health,
            inventory: Inventory::default(),
        };
        let bytes = encode(&welcome, MAX_FRAME).unwrap();
        let ServerMessage::Welcome {
            health: decoded, ..
        } = decode(&bytes, MAX_FRAME).unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(decoded, health);
        let player = PlayerSnapshot {
            id: 1,
            last_input: 3,
            state: PlayerState::default(),
            health,
            life: 0,
            yaw: 0.0,
        };
        let snapshot = Snapshot {
            tick: 9,
            you: player.clone(),
            players: vec![player],
            actors: vec![],
        };
        let bytes = encode(&snapshot, MAX_DATAGRAM).unwrap();
        let decoded: Snapshot = decode(&bytes, MAX_DATAGRAM).unwrap();
        assert_eq!(decoded.you.health.current(), 63);
        assert_eq!(decoded.players[0].health, health);
    }

    #[test]
    fn health_decode_rejects_invalid_bounds() {
        // Field order matches Health's serialized current/maximum pair.
        for values in [(1_u16, 0_u16), (101, 100)] {
            let bytes = encode(&values, MAX_DATAGRAM).unwrap();
            assert!(decode::<Health>(&bytes, MAX_DATAGRAM).is_err());
        }
        let bytes = encode(&(0_u16, 100_u16), MAX_DATAGRAM).unwrap();
        assert!(
            decode::<Health>(&bytes, MAX_DATAGRAM)
                .unwrap()
                .is_depleted()
        );
    }

    #[test]
    fn maximum_player_snapshot_fits_one_datagram_with_health() {
        let player = PlayerSnapshot {
            id: u64::MAX,
            last_input: u64::MAX,
            state: PlayerState::default(),
            health: Health::new(u16::MAX).unwrap(),
            life: u64::MAX,
            yaw: 1.0,
        };
        let snapshot = Snapshot {
            tick: u64::MAX,
            you: player.clone(),
            players: vec![player; MAX_PLAYERS - 1],
            actors: vec![],
        };
        let bytes = encode(&snapshot, MAX_DATAGRAM).unwrap();
        let decoded: Snapshot = decode(&bytes, MAX_DATAGRAM).unwrap();
        assert_eq!(decoded.players.len(), MAX_PLAYERS - 1);
        assert_eq!(decoded.you.health.maximum(), u16::MAX);
    }
    #[test]
    fn strict_decode_rejects_trailing_bytes_and_size_limit() {
        let mut bytes = encode(&ClientMessage::Hello { version: 1 }, MAX_FRAME).unwrap();
        assert!(decode::<ClientMessage>(&bytes, bytes.len() - 1).is_err());
        bytes.push(0);
        assert!(decode::<ClientMessage>(&bytes, MAX_FRAME).is_err());
    }
    #[test]
    fn bounded_physics_snapshot_round_trips_in_one_reliable_frame() {
        let bodies = (0..MAX_PHYSICS_BODIES)
            .map(|id| PhysicsBodySnapshot {
                id: id as u32,
                position: Vec3::new(0.5, 20.5, -0.5),
                velocity: Vec3::new(1.0, -2.0, 3.0),
                material: 3,
            })
            .collect();
        let bytes = encode(&ServerMessage::Physics { tick: 600, bodies }, MAX_FRAME).unwrap();
        let ServerMessage::Physics { tick, bodies } = decode(&bytes, MAX_FRAME).unwrap() else {
            panic!("wrong message")
        };
        assert_eq!(tick, 600);
        assert_eq!(bodies.len(), MAX_PHYSICS_BODIES);
        assert_eq!(bodies[127].position, Vec3::new(0.5, 20.5, -0.5));
        assert_eq!(bodies[127].velocity, Vec3::new(1.0, -2.0, 3.0));
    }

    #[test]
    fn life_tokens_round_trip_in_snapshot_and_input_packet() {
        let player = PlayerSnapshot {
            id: 4,
            last_input: 9,
            state: PlayerState::default(),
            health: Health::default(),
            life: 3,
            yaw: 0.5,
        };
        let snapshot = Snapshot {
            tick: 12,
            you: player.clone(),
            players: vec![player],
            actors: vec![],
        };
        let bytes = encode(&snapshot, MAX_DATAGRAM).unwrap();
        let decoded: Snapshot = decode(&bytes, MAX_DATAGRAM).unwrap();
        assert_eq!(decoded.you.life, 3);
        assert_eq!(decoded.players[0].life, 3);

        let packet = InputPacket {
            session: 7,
            life: 3,
            inputs: vec![PlayerInput::default()],
        };
        let bytes = encode(&packet, MAX_DATAGRAM).unwrap();
        let decoded: InputPacket = decode(&bytes, MAX_DATAGRAM).unwrap();
        assert_eq!(decoded.session, 7);
        assert_eq!(decoded.life, 3);
    }

    #[test]
    fn respawn_request_and_dead_rejection_round_trip() {
        let bytes = encode(&ClientMessage::Respawn { life: 5 }, MAX_FRAME).unwrap();
        let decoded: ClientMessage = decode(&bytes, MAX_FRAME).unwrap();
        assert!(matches!(decoded, ClientMessage::Respawn { life: 5 }));

        let bytes = encode(&Some(EditRejection::Dead), MAX_FRAME).unwrap();
        let decoded: Option<EditRejection> = decode(&bytes, MAX_FRAME).unwrap();
        assert_eq!(decoded, Some(EditRejection::Dead));
    }

    #[test]
    fn inventory_and_drops_round_trip_in_one_frame() {
        let mut inventory = Inventory::default();
        assert_eq!(inventory.add(3, 12), 12);
        let bytes = encode(&ServerMessage::Inventory { inventory }, MAX_FRAME).unwrap();
        let ServerMessage::Inventory {
            inventory: decoded, ..
        } = decode(&bytes, MAX_FRAME).unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(decoded, inventory);
        assert_eq!(decoded.count(3), 12);

        let drops = (0..MAX_DROPS)
            .map(|id| DropSnapshot {
                id: id as u32,
                item: 3,
                count: 1,
                position: Vec3::new(0.5, 20.5, -0.5),
            })
            .collect();
        let bytes = encode(&ServerMessage::Drops { tick: 9, drops }, MAX_FRAME).unwrap();
        let ServerMessage::Drops { tick, drops } = decode(&bytes, MAX_FRAME).unwrap() else {
            panic!("wrong message")
        };
        assert_eq!(tick, 9);
        assert_eq!(drops.len(), MAX_DROPS);
        assert_eq!(drops[MAX_DROPS - 1].position, Vec3::new(0.5, 20.5, -0.5));
    }

    #[test]
    fn inventory_decode_rejects_counts_above_the_stack_ceiling() {
        let bytes = encode(&[1_u16, 0, 0, 0, 0, 0, 0, 0, 0, MAX_STACK], MAX_FRAME).unwrap();
        assert!(decode::<Inventory>(&bytes, MAX_FRAME).is_ok());
        let bytes = encode(&[1_u16, 0, 0, 0, 0, 0, 0, 0, 0, MAX_STACK + 1], MAX_FRAME).unwrap();
        assert!(decode::<Inventory>(&bytes, MAX_FRAME).is_err());
    }

    #[test]
    fn chunk_and_delta_round_trip_voxel_state() {
        let chunk = ServerMessage::Chunk {
            coord: IVec3::new(-1, 2, 3),
            revision: 41,
            material_runs: vec![(32760, 3), (1, 0x80 | 5), (7, 0)],
            density_runs: vec![(32760, 127), (8, -128)],
        };
        let bytes = encode(&chunk, MAX_FRAME).unwrap();
        let ServerMessage::Chunk {
            material_runs,
            density_runs,
            ..
        } = decode(&bytes, MAX_FRAME).unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(material_runs[1], (1, 0x85));
        assert_eq!(density_runs, vec![(32760, 127), (8, -128)]);

        let delta = ServerMessage::Delta {
            coord: IVec3::ZERO,
            from: 41,
            to: 43,
            voxels: vec![
                (
                    7,
                    Voxel {
                        material: 0,
                        density: -40,
                        placed: false,
                    },
                ),
                (
                    9,
                    Voxel {
                        material: 5,
                        density: -128,
                        placed: true,
                    },
                ),
            ],
        };
        let bytes = encode(&delta, MAX_FRAME).unwrap();
        let ServerMessage::Delta { from, to, voxels, .. } =
            decode(&bytes, MAX_FRAME).unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!((from, to), (41, 43));
        assert!(voxels[1].1.placed && voxels[1].1.density == -128);
    }
}
