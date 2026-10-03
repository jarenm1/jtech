mod bow_power;
pub use bow_power::BowPower;
use controller::{CharacterState, PlayerInput};
pub use gameplay::{Health, Inventory};
use glam::{IVec3, Vec3};
use physics::PlayerState;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{io, sync::Arc};

pub const PROTOCOL_VERSION: u32 = 22;
pub const MAX_PHYSICS_BODIES: usize = 128;
pub const MAX_PLAYERS: usize = 16;
pub const MAX_DATAGRAM: usize = 1200;
pub const MAX_FRAME: usize = 128 * 1024;
pub const MAX_ARROWS: usize = 32;
/// Ceiling for one complete dropped-item replication, matching the server bound.
pub const MAX_DROPS: usize = 64;
/// Item id of the package-loaded explosive bow. It is not a carried stack:
/// every client may hotbar it regardless of inventory contents. Item ids are
/// u32: ids 0–5 are hands and block materials, the bow claims 6, and packages
/// share the wide namespace above it.
pub const EXPLOSIVE_BOW_ITEM: u32 = 6;
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
    OutOfStock,
}


/// One voxel's replicated state. Material bytes in chunk runs carry the
/// placed flag in the high bit; deltas send it as a plain field instead.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Voxel {
    pub material: u8,
    pub density: i8,
    pub placed: bool,
}

/// One placed organic scatter instance. `y` is the rendered surface height, so
/// a model authored with its base at the origin sits on the ground.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ScatterInstance {
    /// Index into the species table announced in `Welcome`.
    pub species: u16,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Rotation about the vertical axis, in radians.
    pub yaw: f32,
    pub scale: f32,
}

/// One scatter species: the model a chunk instance index refers to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScatterSpeciesInfo {
    pub name: String,
    /// Package owning the model, e.g. `terrain`.
    pub package: String,
    /// Model path relative to the package's `assets/` directory.
    pub model: String,
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
    /// Request one package asset file announced in the Packages manifest.
    AssetRequest {
        package: String,
        path: String,
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
        /// Scatter species table; chunk instances index into it.
        scatter_species: Vec<ScatterSpeciesInfo>,
    },
    Chunk {
        coord: IVec3,
        revision: u64,
        /// Material runs with the placed flag packed into the high bit. Shared
        /// across recipients of the same chunk revision; serializes as a Vec.
        material_runs: Arc<Vec<(u16, u8)>>,
        density_runs: Arc<Vec<(u16, i8)>>,
        /// Organic scatter anchored inside this chunk, sent once with it.
        scatter: Vec<ScatterInstance>,
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
        /// Merged melee weapon table across loaded melee packages.
        melee_weapons: Vec<MeleeWeaponInfo>,
        /// Files shipped by loaded packages under `assets/`, for client download.
        assets: Vec<PackageAssetInfo>,
    },
    /// One chunk of a requested package asset; `offset` orders reassembly and
    /// `total` is the full file size. Chunks arrive reliably in order.
    AssetData {
        package: String,
        path: String,
        offset: u32,
        total: u32,
        data: Vec<u8>,
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

/// How an item behaves in inventories: stackable resources merge counts;
/// equipment is unique — a player carries at most one and it drops on death.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ItemKind {
    Stack,
    Equipment,
}

/// One authored melee weapon as clients need it: display name plus the swing
/// parameters used for crosshair routing and future combat UI.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MeleeWeaponInfo {
    pub item: u32,
    /// Package that authored this weapon; namespaces its asset paths.
    pub package: String,
    pub name: String,
    pub kind: ItemKind,
    pub range: f32,
    pub damage: u16,
    pub cooldown_ticks: u32,
    pub knockback: f32,
    /// Asset path relative to the owning package's `assets/` dir, if the weapon
    /// ships a 3D model.
    pub model: Option<String>,
}

/// One file a loaded package ships under its `assets/` directory. `hash`
/// versions the content so clients can cache by content, not name.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageAssetInfo {
    pub package: String,
    /// Path relative to the package's `assets/` directory, e.g. `knife.glb`.
    pub path: String,
    pub size: u32,
    pub hash: u64,
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
    pub item: u32,
    pub count: u32,
    pub position: Vec3,
}

/// The local player's full state at 20 Hz: complete `CharacterState` so
/// reconciliation can replay statuses, cooldowns, casts and dashes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub id: u64,
    pub last_input: u64,
    pub state: CharacterState,
    pub health: Health,
    pub life: u64,
}
/// A remote player at 20 Hz: motion and facing only. Remote clients interpolate
/// position and yaw, so the full controller state would not fit a datagram.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemotePlayerSnapshot {
    pub id: u64,
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
    pub players: Vec<RemotePlayerSnapshot>,
    pub actors: Vec<ActorSnapshot>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputPacket {
    pub session: u64,
    pub life: u64,
    pub inputs: InputBatch,
}

/// Bounded redundant input tail: at most [`MAX_INPUT_BATCH`] unacknowledged
/// inputs per datagram. Stored inline so neither the client send path nor the
/// server decode path allocates a `Vec` for a payload bounded at 8 entries.
pub const MAX_INPUT_BATCH: usize = 8;

#[derive(Clone, Copy, Debug, Default)]
pub struct InputBatch {
    inputs: [PlayerInput; MAX_INPUT_BATCH],
    len: u8,
}

impl InputBatch {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.len as usize
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn as_slice(&self) -> &[PlayerInput] {
        &self.inputs[..self.len as usize]
    }
    /// Append one input; returns false when the batch is full.
    pub fn push(&mut self, input: PlayerInput) -> bool {
        if self.len as usize >= MAX_INPUT_BATCH {
            return false;
        }
        self.inputs[self.len as usize] = input;
        self.len += 1;
        true
    }
}

impl std::ops::Deref for InputBatch {
    type Target = [PlayerInput];
    fn deref(&self) -> &[PlayerInput] {
        self.as_slice()
    }
}

impl From<&[PlayerInput]> for InputBatch {
    fn from(inputs: &[PlayerInput]) -> Self {
        let mut batch = Self::default();
        for &input in inputs.iter().take(MAX_INPUT_BATCH) {
            batch.push(input);
        }
        batch
    }
}

impl From<Vec<PlayerInput>> for InputBatch {
    fn from(inputs: Vec<PlayerInput>) -> Self {
        Self::from(inputs.as_slice())
    }
}

impl FromIterator<PlayerInput> for InputBatch {
    fn from_iter<I: IntoIterator<Item = PlayerInput>>(iter: I) -> Self {
        let mut batch = Self::default();
        for input in iter {
            batch.push(input);
        }
        batch
    }
}

// Serialized as a postcard sequence of the live prefix, identical to the
// previous `Vec<PlayerInput>` wire encoding.
impl Serialize for InputBatch {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for input in self.as_slice() {
            seq.serialize_element(input)?;
        }
        seq.end()
    }
}

impl<'de> Deserialize<'de> for InputBatch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error, SeqAccess, Visitor};
        use std::fmt;
        struct BatchVisitor;
        impl<'de> Visitor<'de> for BatchVisitor {
            type Value = InputBatch;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "at most {MAX_INPUT_BATCH} player inputs")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<InputBatch, A::Error> {
                let mut batch = InputBatch::default();
                while let Some(input) = seq.next_element::<PlayerInput>()? {
                    if !batch.push(input) {
                        return Err(A::Error::custom("input batch exceeds limit"));
                    }
                }
                Ok(batch)
            }
        }
        deserializer.deserialize_seq(BatchVisitor)
    }
}

pub fn encode<T: Serialize>(value: &T, limit: usize) -> io::Result<Vec<u8>> {
    let bytes = postcard::to_stdvec(value).map_err(invalid)?;
    if bytes.len() > limit {
        return Err(invalid("encoded message exceeds limit"));
    }
    Ok(bytes)
}

/// Serialize into `output` (appending) instead of allocating. On error `output`
/// is restored to its original length so the buffer stays reusable.
pub fn encode_into<T: Serialize>(value: &T, limit: usize, output: &mut Vec<u8>) -> io::Result<()> {
    let start = output.len();
    if let Err(error) = postcard::to_io(value, &mut *output) {
        output.truncate(start);
        return Err(invalid(error));
    }
    if output.len() - start > limit {
        output.truncate(start);
        return Err(invalid("encoded message exceeds limit"));
    }
    Ok(())
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
            inventory: Inventory::default(),
            scatter_species: vec![ScatterSpeciesInfo {
                name: "oak".into(),
                package: "terrain".into(),
                model: "oak.glb".into(),
            }],
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
            state: CharacterState::default(),
            health,
            life: 0,
        };
        let remote = RemotePlayerSnapshot {
            id: 1,
            state: PlayerState::default(),
            health,
            life: 0,
            yaw: 0.0,
        };
        let snapshot = Snapshot {
            tick: 9,
            you: player,
            players: vec![remote],
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
            state: CharacterState::default(),
            health: Health::new(u16::MAX).unwrap(),
            life: u64::MAX,
        };
        let remote = RemotePlayerSnapshot {
            id: u64::MAX,
            state: PlayerState::default(),
            health: Health::new(u16::MAX).unwrap(),
            life: u64::MAX,
            yaw: 1.0,
        };
        let snapshot = Snapshot {
            tick: u64::MAX,
            you: player,
            players: vec![remote; MAX_PLAYERS - 1],
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
            state: CharacterState::default(),
            health: Health::default(),
            life: 3,
        };
        let remote = RemotePlayerSnapshot {
            id: 4,
            state: PlayerState::default(),
            health: Health::default(),
            life: 3,
            yaw: 0.5,
        };
        let snapshot = Snapshot {
            tick: 12,
            you: player,
            players: vec![remote],
            actors: vec![],
        };
        let bytes = encode(&snapshot, MAX_DATAGRAM).unwrap();
        let decoded: Snapshot = decode(&bytes, MAX_DATAGRAM).unwrap();
        assert_eq!(decoded.you.life, 3);
        assert_eq!(decoded.players[0].life, 3);

        let packet = InputPacket {
            session: 7,
            life: 3,
            inputs: vec![PlayerInput::default()].into(),
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
        let bytes = encode(
            &ServerMessage::Inventory {
                inventory: inventory.clone(),
            },
            MAX_FRAME,
        )
        .unwrap();
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
    fn inventory_decode_rejects_malformed_stacks() {
        // Item 0 and zero counts are not valid inventories; duplicate ids are
        // legal because equipment occupies one entry per instance.
        for entries in [vec![(0_u32, 5_u32)], vec![(3, 0)]] {
            let bytes = encode(&entries, MAX_FRAME).unwrap();
            assert!(decode::<Inventory>(&bytes, MAX_FRAME).is_err());
        }
        let bytes = encode(&vec![(3_u32, 5_u32), (7, 1), (7, 1)], MAX_FRAME).unwrap();
        assert!(decode::<Inventory>(&bytes, MAX_FRAME).is_ok());
    }

    #[test]
    fn chunk_and_delta_round_trip_voxel_state() {
        let chunk = ServerMessage::Chunk {
            coord: IVec3::new(-1, 2, 3),
            revision: 41,
            material_runs: Arc::new(vec![(32760, 3), (1, 0x80 | 5), (7, 0)]),
            density_runs: Arc::new(vec![(32760, 127), (8, -128)]),
            scatter: vec![ScatterInstance {
                species: 2,
                x: 1.5,
                y: 44.0,
                z: -3.25,
                yaw: 1.25,
                scale: 1.1,
            }],
        };
        let bytes = encode(&chunk, MAX_FRAME).unwrap();
        let ServerMessage::Chunk {
            material_runs,
            density_runs,
            scatter,
            ..
        } = decode(&bytes, MAX_FRAME).unwrap()
        else {
            panic!("wrong message")
        };
        assert_eq!(material_runs[1], (1, 0x85));
        assert_eq!(*density_runs, vec![(32760, 127), (8, -128)]);
        assert_eq!(scatter.len(), 1);
        assert_eq!(scatter[0].species, 2);
        assert_eq!(scatter[0].z, -3.25);

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
