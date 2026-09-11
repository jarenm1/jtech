mod bow_power;
pub use bow_power::BowPower;
pub use gameplay::Health;
use glam::{IVec3, Vec3};
use physics::{PlayerInput, PlayerState};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io;

pub const PROTOCOL_VERSION: u32 = 11;
pub const MAX_PHYSICS_BODIES: usize = 128;
pub const MAX_PLAYERS: usize = 16;
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_FRAME: usize = 128 * 1024;
pub const MAX_ARROWS: usize = 32;
pub const EXPLOSIVE_BOW_SLOT: u8 = 6;
pub const EXPLOSIVE_BOW_SHOTS_PER_SECOND: u32 = 25;
/// Horizontal chunk radius shared by server configuration and client camera bounds.
pub const DEFAULT_VIEW_RADIUS: i32 = 16;
pub const MAX_VIEW_RADIUS: i32 = 64;

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
    },
    Chunk {
        coord: IVec3,
        revision: u64,
        runs: Vec<(u16, u8)>,
    },
    Delta {
        coord: IVec3,
        from: u64,
        to: u64,
        local_index: u16,
        block: u8,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub id: u64,
    pub last_input: u64,
    pub state: PlayerState,
    pub health: Health,
    pub life: u64,
    pub yaw: f32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub tick: u64,
    pub you: PlayerSnapshot,
    pub players: Vec<PlayerSnapshot>,
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
}
