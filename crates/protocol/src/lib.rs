use glam::{IVec3, Vec3};
use physics::{PlayerInput, PlayerState};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io;

pub const PROTOCOL_VERSION: u32 = 4;
pub const MAX_PHYSICS_BODIES: usize = 128;
pub const MAX_PLAYERS: usize = 16;
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_FRAME: usize = 128 * 1024;

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
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome {
        id: u64,
        session: u64,
        seed: u64,
        spawn: PlayerState,
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
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PhysicsBodySnapshot {
    pub id: u32,
    pub position: Vec3,
    pub velocity: Vec3,
    pub material: u8,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub id: u64,
    pub last_input: u64,
    pub state: PlayerState,
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
}
