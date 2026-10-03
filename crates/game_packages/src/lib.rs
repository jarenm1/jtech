//! Server-side Steel packages. Native simulation owns entities and physics;
//! Scheme authors validated native policies during package loading.
//!
//! Every subdirectory of the packages root (except the startup-only terrain
//! package) is a live-reloadable package slot. Each `server.scm` declares a
//! `package-kind`; the host compiles it into the matching native policy and
//! merges contributions into shared views the simulation reads. Two packages
//! may both author melee weapons: their item ids merge into one table, and a
//! conflicting id rejects the later package in sorted order.
mod budget;
mod terrain;

pub use terrain::{TERRAIN_PACKAGE, load_terrain};

use budget::Budget;
use gameplay::combat::MeleeSpec;
use parking_lot::Mutex;
use protocol::{BowPower, PackageState, PackageStatus};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use steel::{SteelVal, steel_vm::engine::Engine};

/// Directory name of the bow package; also the id clients display for it.
pub const BOW_PACKAGE: &str = "explosive-bow";

const MAX_SOURCE_BYTES: u64 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CALLBACK_BUDGET: Duration = Duration::from_millis(20);
const LOAD_BUDGET: Duration = Duration::from_secs(5);
const API: &str = "
(define (projectile speed gravity travel lifetime) (list speed gravity travel lifetime))
(define (explosion radius energy player-speed absorbed pulse)
  (list radius energy player-speed absorbed pulse))
(define (melee-weapon id name range damage cooldown-ticks knockback . model)
  (list id name range damage cooldown-ticks knockback model))
";

/// Prefer packages beside the executable, then the working directory. The final
/// development fallback supports `cargo test` from individual workspace crates.
pub fn default_directory() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
        && parent.join("packages").is_dir()
    {
        return parent.join("packages");
    }
    let local = PathBuf::from("packages");
    if local.is_dir() {
        return local;
    }
    let development = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
    if development.is_dir() {
        development
    } else {
        local
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProjectileSpec {
    pub speed: f32,
    pub gravity: f32,
    pub max_travel: f32,
    pub max_age_ticks: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlastSpec {
    pub radius: f32,
    pub energy: f32,
    pub player_speed: f32,
    pub absorbed_fraction: f32,
    pub load_window: f32,
}

struct Vm {
    engine: Engine,
    budget: Budget,
}

impl Vm {
    fn new() -> Result<Self, String> {
        let engine = Engine::new_sandboxed();
        let budget = Budget::new(&engine)
            .map_err(|error| format!("cannot start Scheme watchdog: {error}"))?;
        Ok(Self { engine, budget })
    }

    fn run<T>(
        &mut self,
        duration: Duration,
        action: impl FnOnce(&mut Engine) -> Result<T, String>,
    ) -> Result<T, String> {
        self.budget.arm(duration);
        let result = action(&mut self.engine);
        let expired = self.budget.disarm();
        if expired {
            Err("Scheme execution budget exceeded".into())
        } else {
            result
        }
    }

    fn callback(&mut self, name: &str, power: BowPower) -> Result<SteelVal, String> {
        self.run(CALLBACK_BUDGET, |engine| {
            engine
                .call_function_by_name_with_args(name, vec![SteelVal::IntV(power as isize)])
                .map_err(|error| diagnostic(engine, error))
        })
    }
}

fn diagnostic(engine: &Engine, error: steel::SteelErr) -> String {
    let fallback = error.to_string();
    engine.raise_error_to_string(error).unwrap_or(fallback)
}

/// A compiled package. The VM is discarded after loading; simulation ticks only
/// read validated native values.
#[derive(Clone)]
enum Package {
    Bow(Arc<BowPackage>),
    Melee(Arc<MeleePackage>),
}

impl Package {
    fn compile(source: String, path: PathBuf, generation: u64) -> Result<Self, String> {
        let package_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut vm = Vm::new()?;
        vm.run(LOAD_BUDGET, |engine| {
            engine.run(API).map_err(|error| diagnostic(engine, error))?;
            engine
                .compile_and_run_raw_program_with_path(source, path)
                .map_err(|error| diagnostic(engine, error))?;
            Ok(())
        })?;
        let api = vm
            .engine
            .extract_value("package-api-version")
            .map_err(|e| e.to_string())?;
        if !matches!(api, SteelVal::IntV(1)) {
            return Err("package-api-version must be 1".into());
        }
        let kind = vm
            .engine
            .extract_value("package-kind")
            .map_err(|_| "package-kind must be a string".to_string())?;
        let SteelVal::StringV(kind) = kind else {
            return Err("package-kind must be a string".into());
        };
        match kind.as_str() {
            "bow" => Ok(Self::Bow(Arc::new(BowPackage::from_vm(
                &mut vm, generation,
            )?))),
            "melee" => Ok(Self::Melee(Arc::new(MeleePackage::from_vm(
                &mut vm,
                generation,
                &package_dir,
            )?))),
            other => Err(format!("unknown package-kind {other:?}")),
        }
    }

    fn generation(&self) -> u64 {
        match self {
            Self::Bow(package) => package.generation,
            Self::Melee(package) => package.generation,
        }
    }
}

/// Immutable policy for the four network power presets.
pub struct BowPackage {
    generation: u64,
    shots_per_second: u32,
    projectiles: [ProjectileSpec; 4],
    blasts: [BlastSpec; 4],
}

impl BowPackage {
    /// Compile a standalone bow package; `package-kind` must be `"bow"`.
    pub fn compile(source: String, path: PathBuf, generation: u64) -> Result<Self, String> {
        match Package::compile(source, path, generation)? {
            Package::Bow(package) => Ok((*package).clone()),
            _ => unreachable!("kind dispatch returned a non-bow package"),
        }
    }

    fn from_vm(vm: &mut Vm, generation: u64) -> Result<Self, String> {
        let rate = vm
            .engine
            .extract_value("shots-per-second")
            .map_err(|e| e.to_string())?;
        let SteelVal::IntV(rate @ 1..=60) = rate else {
            return Err("shots-per-second must be an integer in 1..=60".into());
        };
        let projectiles = four(BowPower::ALL.map(|power| {
            vm.callback("projectile-for-power", power)
                .and_then(projectile_spec)
        }))?;
        let blasts = four(
            BowPower::ALL.map(|power| vm.callback("blast-for-power", power).and_then(blast_spec)),
        )?;
        Ok(Self {
            generation,
            shots_per_second: rate as u32,
            projectiles,
            blasts,
        })
    }

    pub fn projectile(&self, power: BowPower) -> ProjectileSpec {
        self.projectiles[power as usize]
    }
    pub fn impact(&self, power: BowPower) -> BlastSpec {
        self.blasts[power as usize]
    }
}

impl Clone for BowPackage {
    fn clone(&self) -> Self {
        Self {
            generation: self.generation,
            shots_per_second: self.shots_per_second,
            projectiles: self.projectiles,
            blasts: self.blasts,
        }
    }
}

fn four<T>(values: [Result<T, String>; 4]) -> Result<[T; 4], String> {
    let [a, b, c, d] = values;
    Ok([a?, b?, c?, d?])
}

fn projectile_spec(value: SteelVal) -> Result<ProjectileSpec, String> {
    let [speed, gravity, max_travel, age] = numbers(value, "projectile-for-power")?;
    bound("speed", speed, 0.1, 200.0)?;
    bound("gravity", gravity, 0.0, 100.0)?;
    bound("max travel", max_travel, 0.1, 256.0)?;
    bound("lifetime ticks", age, 1.0, 3600.0)?;
    if age.fract() != 0.0 {
        return Err("lifetime ticks must be an integer".into());
    }
    Ok(ProjectileSpec {
        speed,
        gravity,
        max_travel,
        max_age_ticks: age as u32,
    })
}

fn blast_spec(value: SteelVal) -> Result<BlastSpec, String> {
    let [radius, energy, player_speed, absorbed_fraction, load_window] =
        numbers(value, "blast-for-power")?;
    bound("radius", radius, 0.1, 12.0)?;
    bound("energy", energy, 0.0, 100_000.0)?;
    bound("player speed", player_speed, 0.0, 100.0)?;
    bound("absorbed fraction", absorbed_fraction, 0.0, 1.0)?;
    bound("load window", load_window, 0.00001, 1.0)?;
    Ok(BlastSpec {
        radius,
        energy,
        player_speed,
        absorbed_fraction,
        load_window,
    })
}

/// A melee weapon one package contributes to the shared item table.
#[derive(Clone, Debug, PartialEq)]
pub struct MeleeWeapon {
    /// Item id; must exceed `protocol::EXPLOSIVE_BOW_ITEM` so it cannot shadow
    /// hands, block materials, or the bow.
    pub id: u32,
    /// Owning package id, assigned when the weapon merges into the table.
    pub package: String,
    /// Display name for logs and a future presentation API.
    pub name: String,
    pub spec: MeleeSpec,
    /// Model file under the package's `assets/` directory, if authored.
    pub model: Option<String>,
}

/// Immutable melee policy: authored weapons plus the items granted to every
/// player on spawn and respawn.
pub struct MeleePackage {
    generation: u64,
    weapons: Vec<MeleeWeapon>,
    spawn_items: Vec<(u32, u32)>,
}

impl MeleePackage {
    fn from_vm(vm: &mut Vm, generation: u64, package_dir: &Path) -> Result<Self, String> {
        let value = vm
            .engine
            .extract_value("weapons")
            .map_err(|_| "weapons must be a list of melee-weapon values".to_string())?;
        let SteelVal::ListV(entries) = value else {
            return Err("weapons must be a list of melee-weapon values".into());
        };
        let mut weapons = Vec::with_capacity(entries.len());
        let mut seen = HashSet::new();
        for entry in entries.iter() {
            let weapon = melee_weapon(entry.clone(), package_dir)?;
            if !seen.insert(weapon.id) {
                return Err(format!("weapons declares item id {} twice", weapon.id));
            }
            weapons.push(weapon);
        }
        let spawn_items = match vm.engine.extract_value("spawn-items") {
            Ok(SteelVal::ListV(entries)) => entries
                .iter()
                .map(|entry| spawn_item(entry.clone()))
                .collect::<Result<_, _>>()?,
            Ok(_) => return Err("spawn-items must be a list of (id count) pairs".into()),
            Err(_) => Vec::new(),
        };
        let registered: HashSet<u32> = weapons.iter().map(|weapon| weapon.id).collect();
        for &(item, _) in &spawn_items {
            if !registered.contains(&item) {
                return Err(format!(
                    "spawn-items grants item {item} that this package does not register"
                ));
            }
        }
        Ok(Self {
            generation,
            weapons,
            spawn_items,
        })
    }
}

fn melee_weapon(value: SteelVal, package_dir: &Path) -> Result<MeleeWeapon, String> {
    let SteelVal::ListV(fields) = value else {
        return Err("each weapon must be a melee-weapon value".into());
    };
    if fields.len() != 7 {
        return Err("melee-weapon takes id, name, range, damage, cooldown-ticks, knockback, optional model".into());
    }
    let id = integer(&fields[0], "weapon id")?;
    let id = u32::try_from(id)
        .ok()
        .filter(|id| *id > protocol::EXPLOSIVE_BOW_ITEM)
        .ok_or_else(|| {
            format!(
                "weapon id must be an integer in {}..={}",
                protocol::EXPLOSIVE_BOW_ITEM + 1,
                u32::MAX
            )
        })?;
    let SteelVal::StringV(name) = &fields[1] else {
        return Err("weapon name must be a string".into());
    };
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect();
    if name.is_empty() {
        return Err("weapon name must not be empty".into());
    }
    let range = number(&fields[2], "weapon range")?;
    let damage = integer(&fields[3], "weapon damage")?;
    let cooldown = integer(&fields[4], "weapon cooldown ticks")?;
    let knockback = number(&fields[5], "weapon knockback")?;
    let spec = MeleeSpec {
        range,
        damage: u16::try_from(damage).unwrap_or(u16::MAX),
        cooldown_ticks: u32::try_from(cooldown).unwrap_or(u32::MAX),
        knockback,
    }
    .bounded()
    .ok_or_else(|| {
        format!(
            "weapon {name} tuning out of range: range 0.1..=16, damage 1..=65535, \
             cooldown 0..=600 ticks, knockback 0..=10000"
        )
    })?;
    let model = weapon_model(&fields[6], package_dir)?;
    Ok(MeleeWeapon {
        id,
        package: String::new(),
        name,
        spec,
        model,
    })
}

/// Optional trailing model argument: a `.glb` path that must exist under the
/// package's `assets/` directory so a typo fails the package, not the client.
fn weapon_model(value: &SteelVal, package_dir: &Path) -> Result<Option<String>, String> {
    let SteelVal::ListV(items) = value else {
        return Err("weapon model must be a \"path.glb\" string or omitted".into());
    };
    if items.is_empty() {
        return Ok(None);
    }
    if items.len() != 1 {
        return Err("weapon model takes a single path".into());
    }
    let SteelVal::StringV(path) = &items[0] else {
        return Err("weapon model path must be a string".into());
    };
    let path = path.as_str();
    let relative = Path::new(path);
    if path.is_empty()
        || relative.is_absolute()
        || relative.components().any(|c| matches!(c, std::path::Component::ParentDir))
        || !path.ends_with(".glb")
    {
        return Err(format!(
            "weapon model {path:?} must be a relative .glb path inside assets/"
        ));
    }
    let file = package_dir.join("assets").join(relative);
    if !file.is_file() {
        return Err(format!("weapon model {path:?} not found in assets/"));
    }
    Ok(Some(path.to_string()))
}

fn spawn_item(value: SteelVal) -> Result<(u32, u32), String> {
    let SteelVal::ListV(pair) = value else {
        return Err("spawn-items entries must be (id count) pairs".into());
    };
    if pair.len() != 2 {
        return Err("spawn-items entries must be (id count) pairs".into());
    }
    let id = integer(&pair[0], "spawn item id")?;
    let count = integer(&pair[1], "spawn item count")?;
    let id = u32::try_from(id)
        .map_err(|_| format!("spawn item id must be an integer in 0..={}", u32::MAX))?;
    // Equipment grants one inventory entry per count; keep the loadout bounded.
    if !(1..=64).contains(&count) {
        return Err("spawn item count must be an integer in 1..=64".into());
    }
    Ok((id, count as u32))
}

fn integer(value: &SteelVal, name: &str) -> Result<isize, String> {
    match value {
        SteelVal::IntV(value) => Ok(*value),
        _ => Err(format!("{name} must be an integer")),
    }
}

fn number(value: &SteelVal, name: &str) -> Result<f32, String> {
    let value = match value {
        SteelVal::IntV(value) => *value as f32,
        SteelVal::NumV(value) => *value as f32,
        _ => return Err(format!("{name} must be a real number")),
    };
    if !value.is_finite() {
        return Err(format!("{name} must be finite"));
    }
    Ok(value)
}

fn numbers<const N: usize>(value: SteelVal, callback: &str) -> Result<[f32; N], String> {
    let SteelVal::ListV(values) = value else {
        return Err(format!("{callback} must return a list of {N} numbers"));
    };
    if values.len() != N {
        return Err(format!("{callback} must return exactly {N} numbers"));
    }
    let mut result = [0.0; N];
    for (output, value) in result.iter_mut().zip(values.iter()) {
        *output = match value {
            SteelVal::IntV(value) => *value as f32,
            SteelVal::NumV(value) => *value as f32,
            _ => return Err(format!("{callback} must return finite real numbers")),
        };
        if !output.is_finite() {
            return Err(format!("{callback} returned a non-finite number"));
        }
    }
    Ok(result)
}

fn bound(name: &str, value: f32, min: f32, max: f32) -> Result<(), String> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        Err(format!("{name} must be finite and in {min}..={max}"))
    } else {
        Ok(())
    }
}

/// Merged melee view across every loaded melee package. Item ids are unique
/// across packages: the host rejects a package whose ids collide with an
/// already-loaded one.
#[derive(Clone, Default)]
pub struct MeleeTable {
    weapons: Arc<HashMap<u32, MeleeWeapon>>,
    spawn_items: Arc<Vec<(u32, u32)>>,
}

impl MeleeTable {
    /// Authored spec for a registered weapon item; `None` for hands, blocks,
    /// the bow, and unknown ids.
    pub fn spec(&self, item: u32) -> Option<MeleeSpec> {
        self.weapons.get(&item).map(|weapon| weapon.spec)
    }

    /// Every registered weapon, for replication to clients.
    pub fn weapons(&self) -> impl Iterator<Item = &MeleeWeapon> {
        self.weapons.values()
    }

    /// Registered weapons are equipment; everything else stacks.
    pub fn kind(&self, item: u32) -> protocol::ItemKind {
        if self.weapons.contains_key(&item) {
            protocol::ItemKind::Equipment
        } else {
            protocol::ItemKind::Stack
        }
    }

    /// Items every player receives on spawn and respawn, merged across packages.
    pub fn spawn_items(&self) -> &[(u32, u32)] {
        &self.spawn_items
    }
}

pub struct Shot {
    pub projectile: ProjectileSpec,
    pub power: BowPower,
    package: Arc<BowPackage>,
}

impl Shot {
    pub fn impact(&self) -> BlastSpec {
        self.package.impact(self.power)
    }
    pub fn generation(&self) -> u64 {
        self.package.generation
    }
}

type LoadResult = Result<Package, String>;

/// One package directory: a watched `server.scm` with its own reload lifecycle.
struct PackageSlot {
    path: PathBuf,
    observed: Result<String, String>,
    /// (len, mtime) captured just before the read that produced `observed`.
    /// An equal stamp means `observed` still matches the file, so polls skip
    /// re-reading `server.scm` until the file moves again.
    stamp: Option<(u64, std::time::SystemTime)>,
    pending: Option<Mutex<mpsc::Receiver<LoadResult>>>,
    pending_since: Option<Instant>,
    active: Option<Package>,
    status: PackageStatus,
}

impl PackageSlot {
    fn new(path: PathBuf, id: String) -> Self {
        Self {
            path,
            observed: Err("not read yet".into()),
            stamp: None,
            pending: None,
            pending_since: None,
            active: None,
            status: PackageStatus {
                id,
                generation: 0,
                state: PackageState::Loading,
                error: None,
            },
        }
    }

    fn begin_reload(&mut self, source: Result<String, String>) {
        self.observed = source.clone();
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                self.set_status(PackageState::Error, Some(error));
                return;
            }
        };
        self.set_status(
            if self.active.is_some() {
                PackageState::Reloading
            } else {
                PackageState::Loading
            },
            None,
        );
        let generation = self.status.generation + 1;
        let path = self.path.clone();
        let (send, receive) = mpsc::sync_channel(1);
        match std::thread::Builder::new()
            .name("scheme-package-load".into())
            .spawn(move || {
                let result = Package::compile(source, path, generation);
                let _ = send.send(result);
            }) {
            Ok(_) => {
                self.pending = Some(Mutex::new(receive));
                self.pending_since = Some(Instant::now());
            }
            Err(error) => self.set_status(
                PackageState::Error,
                Some(format!("cannot start package loader: {error}")),
            ),
        }
    }

    /// Record a compiled candidate. Conflict checks ran in the host before this.
    fn install(&mut self, package: Package) {
        self.status.generation = package.generation();
        self.active = Some(package);
        self.set_status(PackageState::Loaded, None);
    }

    fn set_status(&mut self, state: PackageState, error: Option<String>) {
        let error = error.map(|text| {
            text.chars()
                .filter(|c| !c.is_control() || *c == '\n')
                .take(1000)
                .collect()
        });
        if self.status.state == state && self.status.error == error && state != PackageState::Loaded
        {
            return;
        }
        self.status.state = state;
        self.status.error = error;
        eprintln!(
            "package {} generation={} {:?}{}",
            self.status.id,
            self.status.generation,
            self.status.state,
            self.status
                .error
                .as_ref()
                .map_or(String::new(), |e| format!(": {e}"))
        );
    }
}

/// Live-reloadable package set. Disk changes compile on background threads; the
/// simulation publishes successful candidates at tick boundaries. Packages are
/// discovered and dropped as directories appear and vanish. Files under each
/// package's `assets/` directory are hashed into a manifest clients download.
pub struct PackageHost {
    directory: PathBuf,
    slots: BTreeMap<String, PackageSlot>,
    next_poll: Instant,
    revision: u64,
    active_bow: Option<Arc<BowPackage>>,
    melee: MeleeTable,
    /// Asset manifest, package id -> relative path -> content fingerprint.
    /// Updated in place so rescans reuse key allocations.
    assets: BTreeMap<String, BTreeMap<String, AssetEntry>>,
    /// Generation counter marking which entries the latest scan touched.
    scan_epoch: u64,
    /// Reusable buffer for the discovered package ids of one poll.
    ids: Vec<String>,
}

struct AssetEntry {
    size: u32,
    hash: u64,
    /// Cheap change detector: rehash only when size or mtime moved.
    stamp: Option<(u64, std::time::SystemTime)>,
    /// Scan generation that last saw this entry; older entries are swept.
    seen: u64,
}

const MAX_ASSET_BYTES: u64 = 16 * 1024 * 1024;

/// FNV-1a: deterministic across builds, unlike `DefaultHasher`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

impl PackageHost {
    pub fn new(directory: &Path) -> Self {
        let mut host = Self {
            directory: directory.to_path_buf(),
            slots: BTreeMap::new(),
            next_poll: Instant::now() + POLL_INTERVAL,
            revision: 0,
            melee: MeleeTable::default(),
            active_bow: None,
            assets: BTreeMap::new(),
            scan_epoch: 0,
            ids: Vec::new(),
        };
        let mut ids = Vec::new();
        discover(directory, &mut ids);
        for id in ids {
            let path = directory.join(&id).join("server.scm");
            let stamp = source_stamp(&path);
            let observed = read_source(&path);
            let result = observed
                .clone()
                .and_then(|source| Package::compile(source, path.clone(), 1));
            let mut slot = PackageSlot::new(path, id);
            slot.observed = observed;
            slot.stamp = stamp;
            host.install_slot(slot, result);
        }
        host.scan_assets();
        host
    }

    /// Statuses of every known package slot, sorted by package id.
    pub fn statuses(&self) -> Vec<PackageStatus> {
        self.slots
            .values()
            .map(|slot| slot.status.clone())
            .collect()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn shots_per_second(&self) -> u32 {
        self.active_bow.as_ref().map_or(0, |p| p.shots_per_second)
    }
    /// Merged melee weapon table across loaded melee packages.
    pub fn melee_table(&self) -> MeleeTable {
        self.melee.clone()
    }

    /// Files shipped by loaded packages, sorted for a stable wire manifest.
    pub fn asset_manifest(&self) -> Vec<protocol::PackageAssetInfo> {
        self.assets
            .iter()
            .filter(|(package, _)| {
                package.as_str() == TERRAIN_PACKAGE
                    || self
                        .slots
                        .get(package.as_str())
                        .is_some_and(|slot| slot.active.is_some())
            })
            .flat_map(|(package, files)| {
                files.iter().map(move |(path, entry)| {
                    protocol::PackageAssetInfo {
                        package: package.clone(),
                        path: path.clone(),
                        size: entry.size,
                        hash: entry.hash,
                    }
                })
            })
            .collect()
    }

    /// Read one manifest asset for a client request. Only files under a loaded
    /// package's `assets/` directory are served.
    pub fn read_asset(&self, package: &str, path: &str) -> Result<Vec<u8>, String> {
        // The startup-only terrain package has no slot but ships scatter models.
        let dir = if package == TERRAIN_PACKAGE {
            self.directory.join(TERRAIN_PACKAGE)
        } else {
            let slot = self
                .slots
                .get(package)
                .ok_or_else(|| format!("unknown package {package}"))?;
            if slot.active.is_none() {
                return Err(format!("package {package} is not loaded"));
            }
            slot.path
                .parent()
                .unwrap_or(Path::new(""))
                .to_path_buf()
        };
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("asset path {path:?} escapes the package"));
        }
        let file = dir.join("assets").join(relative);
        let meta = std::fs::metadata(&file)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        if meta.len() > MAX_ASSET_BYTES {
            return Err(format!("{} exceeds {MAX_ASSET_BYTES} bytes", file.display()));
        }
        std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))
    }

    /// Rehash `assets/` contents; bumps the manifest revision on any change so
    /// clients re-sync and re-download what moved.
    fn scan_assets(&mut self) {
        self.scan_epoch += 1;
        let epoch = self.scan_epoch;
        let mut changed = false;
        // The startup-only terrain package ships scatter models but has no slot.
        let ids = self
            .slots
            .keys()
            .map(String::as_str)
            .chain(std::iter::once(TERRAIN_PACKAGE));
        for id in ids {
            if !self.assets.contains_key(id) {
                self.assets.insert(id.to_string(), BTreeMap::new());
            }
            let entries = self.assets.get_mut(id).unwrap();
            let root = self.directory.join(id).join("assets");
            scan_asset_dir(&root, &root, entries, epoch, &mut changed);
        }
        // Sweep files that vanished or stopped scanning since the last pass.
        let mut removed = 0usize;
        self.assets.retain(|_, files| {
            files.retain(|_, entry| {
                let keep = entry.seen == epoch;
                if !keep {
                    removed += 1;
                }
                keep
            });
            !files.is_empty()
        });
        if changed || removed > 0 {
            self.revision += 1;
        }
    }

    pub fn fire(&self, power: BowPower) -> Result<Shot, String> {
        let package = self
            .active_bow
            .clone()
            .ok_or_else(|| "no bow package is loaded".to_string())?;
        Ok(Shot {
            projectile: package.projectile(power),
            power,
            package,
        })
    }

    /// Call before admitting simulation commands. Returns true when the active
    /// bow firing rate changed, so callers can reset deadlines expressed in
    /// rate units.
    pub fn poll(&mut self) -> bool {
        let old_rate = self.shots_per_second();
        let mut ready = Vec::new();
        for (id, slot) in &mut self.slots {
            let result = slot
                .pending
                .as_ref()
                .and_then(|receiver| match receiver.lock().try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Err("package loader stopped unexpectedly".into()))
                    }
                });
            if let Some(result) = result {
                slot.pending = None;
                slot.pending_since = None;
                // A second save during compilation supersedes this candidate.
                // Stat first: an unchanged stamp still means `observed` matches
                // the file, so most completions skip the read entirely.
                let stamp = source_stamp(&slot.path);
                if stamp == slot.stamp && slot.observed.is_ok() {
                    ready.push((id.clone(), result));
                    continue;
                }
                let current = read_source(&slot.path);
                slot.stamp = stamp;
                if current == slot.observed {
                    ready.push((id.clone(), result));
                } else {
                    slot.begin_reload(current);
                }
            }
        }
        for (id, result) in ready {
            self.install_result(&id, result);
        }
        if Instant::now() >= self.next_poll {
            self.next_poll = Instant::now() + POLL_INTERVAL;
            self.scan_assets();
            self.ids.clear();
            discover(&self.directory, &mut self.ids);
            for id in &self.ids {
                if !self.slots.contains_key(id.as_str()) {
                    let path = self.directory.join(id).join("server.scm");
                    let mut slot = PackageSlot::new(path.clone(), id.clone());
                    slot.stamp = source_stamp(&path);
                    slot.begin_reload(read_source(&path));
                    self.revision += 1;
                    self.slots.insert(id.clone(), slot);
                }
            }
            let slots = self.slots.len();
            self.slots
                .retain(|id, _| self.directory.join(id).is_dir());
            self.revision += (slots - self.slots.len()) as u64;
            for slot in self.slots.values_mut() {
                if slot.pending.is_none() {
                    let stamp = source_stamp(&slot.path);
                    if stamp == slot.stamp && slot.observed.is_ok() {
                        continue;
                    }
                    let source = read_source(&slot.path);
                    slot.stamp = stamp;
                    if source != slot.observed {
                        slot.begin_reload(source);
                    }
                }
            }
        }
        let mut deadline_hit = false;
        for slot in self.slots.values_mut() {
            if slot
                .pending_since
                .is_some_and(|started| started.elapsed() > LOAD_BUDGET + Duration::from_secs(1))
            {
                slot.set_status(PackageState::Error, Some(
                    "Loader exceeded its deadline. If it does not recover, fix the package and restart the server.".into(),
                ));
                deadline_hit = true;
            }
        }
        if deadline_hit {
            self.revision += 1;
        }
        old_rate != self.shots_per_second()
    }

    fn install_slot(&mut self, slot: PackageSlot, result: LoadResult) {
        let id = slot.status.id.clone();
        self.slots.insert(id.clone(), slot);
        self.install_result(&id, result);
    }

    fn install_result(&mut self, id: &str, result: LoadResult) {
        match result {
            Ok(package) => {
                if let Err(error) = self.check_conflicts(id, &package) {
                    self.slots
                        .get_mut(id)
                        .unwrap()
                        .set_status(PackageState::Error, Some(error));
                } else {
                    self.slots.get_mut(id).unwrap().install(package);
                }
            }
            Err(error) => self
                .slots
                .get_mut(id)
                .unwrap()
                .set_status(PackageState::Error, Some(error)),
        }
        self.revision += 1;
        self.rebuild_views();
    }

    /// Item ids are a shared namespace: a melee package may not claim an id an
    /// already-loaded package owns, and only one bow package may be active.
    /// Sorted slot order makes the winner deterministic; the loser reports an
    /// error until its ids change or the winner unloads.
    fn check_conflicts(&self, id: &str, package: &Package) -> Result<(), String> {
        match package {
            Package::Bow(_) => self
                .slots
                .iter()
                .find(|(other, slot)| {
                    *other != id && matches!(slot.active, Some(Package::Bow(_)))
                })
                .map_or(Ok(()), |(other, _)| {
                    Err(format!("bow already provided by package {other}"))
                }),
            Package::Melee(melee) => {
                for (other, slot) in &self.slots {
                    if *other == id {
                        continue;
                    }
                    let Some(Package::Melee(active)) = &slot.active else {
                        continue;
                    };
                    for weapon in &melee.weapons {
                        if let Some(claimed) = active
                            .weapons
                            .iter()
                            .find(|claimed| claimed.id == weapon.id)
                        {
                            return Err(format!(
                                "item id {} ({}) already provided by package {other} ({})",
                                weapon.id, weapon.name, claimed.name
                            ));
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn rebuild_views(&mut self) {
        self.active_bow = None;
        let mut weapons = HashMap::new();
        let mut spawn_items = Vec::new();
        for slot in self.slots.values() {
            match &slot.active {
                Some(Package::Bow(package)) => self.active_bow = Some(package.clone()),
                Some(Package::Melee(package)) => {
                    for weapon in &package.weapons {
                        weapons.insert(
                            weapon.id,
                            MeleeWeapon {
                                package: slot.status.id.clone(),
                                ..weapon.clone()
                            },
                        );
                    }
                    spawn_items.extend_from_slice(&package.spawn_items);
                }
                None => {}
            }
        }
        self.melee = MeleeTable {
            weapons: Arc::new(weapons),
            spawn_items: Arc::new(spawn_items),
        };
    }
}

/// Recursively refresh `entries` with the files under one package's `assets/`
/// directory. Entries keep their keys and hashes between scans: a file is only
/// re-read when its (len, mtime) stamp moved, and entries not seen this epoch
/// are swept by the caller.
fn scan_asset_dir(
    root: &Path,
    dir: &Path,
    entries: &mut BTreeMap<String, AssetEntry>,
    epoch: u64,
    changed: &mut bool,
) {
    let Ok(iter) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in iter.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            scan_asset_dir(root, &path, entries, epoch, changed);
            continue;
        }
        if !meta.is_file() || meta.len() > MAX_ASSET_BYTES {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let stamp = meta.modified().ok().map(|mtime| (meta.len(), mtime));
        // Keys store '/' separators; borrow the name when it needs no fixups.
        let name = relative.to_string_lossy();
        let key: Cow<str> = if name.contains('\\') {
            Cow::Owned(name.replace('\\', "/"))
        } else {
            name
        };
        if let Some(old) = entries.get_mut(key.as_ref()) {
            if old.stamp == stamp {
                old.seen = epoch;
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let size = meta.len() as u32;
            let hash = fnv1a(&bytes);
            if old.size != size || old.hash != hash {
                *changed = true;
            }
            *old = AssetEntry {
                size,
                hash,
                stamp,
                seen: epoch,
            };
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        entries.insert(
            key.into_owned(),
            AssetEntry {
                size: meta.len() as u32,
                hash: fnv1a(&bytes),
                stamp,
                seen: epoch,
            },
        );
        *changed = true;
    }
}

/// Package directories eligible for live hosting: every subdirectory except the
/// startup-only terrain package.
fn discover(directory: &Path, out: &mut Vec<String>) {
    out.extend(
        std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|id| id != TERRAIN_PACKAGE),
    );
    out.sort();
}

/// (len, mtime) of a `server.scm`, or `None` when it cannot be statted. The
/// stamp is taken *before* reading so a mid-read save still compares unequal on
/// the next poll instead of leaving `observed` ahead of its stamp.
fn source_stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

fn read_source(path: &Path) -> Result<String, String> {
    let mut source = String::new();
    File::open(path)
        .and_then(|file| file.take(MAX_SOURCE_BYTES + 1).read_to_string(&mut source))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if source.len() as u64 > MAX_SOURCE_BYTES {
        return Err(format!(
            "{} exceeds {MAX_SOURCE_BYTES} bytes",
            path.display()
        ));
    }
    Ok(source)
}

#[cfg(test)]
mod tests;
