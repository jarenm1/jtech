use glam::IVec3;
use physics::{PlayerInput, PlayerState};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io;

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_PLAYERS: usize = 16;
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_FRAME: usize = 128 * 1024;

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
    },
    Disconnect {
        reason: String,
    },
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
}
