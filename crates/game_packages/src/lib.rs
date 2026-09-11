//! Server-side Steel packages. Native simulation owns entities and physics;
//! Scheme authors validated native weapon policies during package loading.
mod budget;

use budget::Budget;
use protocol::{BowPower, PackageState, PackageStatus};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use steel::{SteelVal, steel_vm::engine::Engine};

pub const BOW_PACKAGE: &str = "explosive-bow";
const MAX_SOURCE_BYTES: u64 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const CALLBACK_BUDGET: Duration = Duration::from_millis(20);
const LOAD_BUDGET: Duration = Duration::from_secs(5);
const API: &str = "
(define (projectile speed gravity travel lifetime) (list speed gravity travel lifetime))
(define (explosion radius energy player-speed absorbed pulse)
  (list radius energy player-speed absorbed pulse))
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
/// Immutable policy for the four network power presets. The VM is discarded
/// after loading; simulation ticks only read validated native values.
pub struct BowPackage {
    generation: u64,
    shots_per_second: u32,
    projectiles: [ProjectileSpec; 4],
    blasts: [BlastSpec; 4],
}

impl BowPackage {
    pub fn compile(source: String, path: PathBuf, generation: u64) -> Result<Self, String> {
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

type LoadResult = Result<Arc<BowPackage>, String>;

/// One server weapon slot in this first package API. Disk changes compile on a
/// background thread; the simulation publishes successful candidates at tick boundaries.
pub struct PackageHost {
    path: PathBuf,
    observed: Result<String, String>,
    next_poll: Instant,
    pending: Option<Mutex<mpsc::Receiver<LoadResult>>>,
    pending_since: Option<Instant>,
    active: Option<Arc<BowPackage>>,
    status: PackageStatus,
    revision: u64,
}

impl PackageHost {
    pub fn new(directory: &Path) -> Self {
        let path = directory.join(BOW_PACKAGE).join("server.scm");
        let observed = read_source(&path);
        let result = observed
            .clone()
            .and_then(|source| BowPackage::compile(source, path.clone(), 1).map(Arc::new));
        let mut host = Self {
            path,
            observed,
            next_poll: Instant::now() + POLL_INTERVAL,
            pending: None,
            pending_since: None,
            active: None,
            revision: 0,
            status: PackageStatus {
                id: BOW_PACKAGE.into(),
                generation: 0,
                state: PackageState::Loading,
                error: None,
            },
        };
        host.install(result);
        host
    }

    pub fn status(&self) -> &PackageStatus {
        &self.status
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn shots_per_second(&self) -> u32 {
        self.active.as_ref().map_or(0, |p| p.shots_per_second)
    }

    pub fn fire(&self, power: BowPower) -> Result<Shot, String> {
        let package = self
            .active
            .clone()
            .ok_or_else(|| "explosive-bow is unavailable".to_string())?;
        Ok(Shot {
            projectile: package.projectile(power),
            power,
            package,
        })
    }

    /// Call before admitting simulation commands. Returns true when the active
    /// firing rate changed, so callers can reset deadlines expressed in rate units.
    pub fn poll(&mut self) -> bool {
        let old_rate = self.shots_per_second();
        let result =
            self.pending
                .as_ref()
                .and_then(|receiver| match receiver.lock().unwrap().try_recv() {
                    Ok(result) => Some(result),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        Some(Err("package loader stopped unexpectedly".into()))
                    }
                });
        if let Some(result) = result {
            self.pending = None;
            self.pending_since = None;
            // A second save during compilation supersedes this candidate.
            let current = read_source(&self.path);
            if current == self.observed {
                self.install(result);
            } else {
                self.begin_reload(current);
            }
        }
        if self.pending.is_none() && Instant::now() >= self.next_poll {
            self.next_poll = Instant::now() + POLL_INTERVAL;
            let source = read_source(&self.path);
            if source != self.observed {
                self.begin_reload(source);
            }
        }
        if self
            .pending_since
            .is_some_and(|started| started.elapsed() > LOAD_BUDGET + Duration::from_secs(1))
        {
            self.set_status(PackageState::Error, Some(
                "Loader exceeded its deadline. If it does not recover, fix the package and restart the server.".into(),
            ));
        }
        old_rate != self.shots_per_second()
    }

    fn begin_reload(&mut self, source: Result<String, String>) {
        self.observed = source.clone();
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                self.install(Err(error));
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
                let result = BowPackage::compile(source, path, generation).map(Arc::new);
                let _ = send.send(result);
            }) {
            Ok(_) => {
                self.pending = Some(Mutex::new(receive));
                self.pending_since = Some(Instant::now());
            }
            Err(error) => self.install(Err(format!("cannot start package loader: {error}"))),
        }
    }

    fn install(&mut self, result: LoadResult) {
        match result {
            Ok(package) => {
                self.status.generation = package.generation;
                self.active = Some(package);
                self.set_status(PackageState::Loaded, None);
            }
            Err(error) => self.set_status(PackageState::Error, Some(error)),
        }
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
        self.revision += 1;
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
