use super::*;
use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
};

const BOW: &str = include_str!("../../../packages/explosive-bow/server.scm");

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "jtech-packages-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join(BOW_PACKAGE)).unwrap();
        Self(root)
    }
    fn write(&self, source: &str) {
        fs::write(self.0.join(BOW_PACKAGE).join("server.scm"), source).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn compile(source: &str) -> Result<BowPackage, String> {
    let fixture = Fixture::new();
    fixture.write(source);
    BowPackage::compile(
        source.to_owned(),
        fixture.0.join(BOW_PACKAGE).join("server.scm"),
        1,
    )
}
fn drain(host: &mut PackageHost) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while host.pending.is_some() {
        assert!(Instant::now() < deadline, "package loader did not finish");
        std::thread::sleep(Duration::from_millis(5));
        host.poll();
    }
}

fn poll_disk(host: &mut PackageHost) {
    host.next_poll = Instant::now();
    host.poll();
}

#[test]
fn bow_package_authors_flight_and_each_blast_preset() {
    let package = compile(BOW).unwrap();
    assert_eq!(package.shots_per_second, 25);
    for (index, power) in BowPower::ALL.into_iter().enumerate() {
        let projectile = package.projectile(power);
        assert_eq!(
            projectile,
            ProjectileSpec {
                speed: 36.0,
                gravity: 3.0,
                max_travel: 64.0,
                max_age_ticks: 180
            }
        );
        let blast = package.impact(power);
        assert_eq!(blast.radius, [3.0, 4.0, 5.0, 6.0][index]);
        assert_eq!(blast.energy, [3000.0, 6000.0, 12000.0, 24000.0][index]);
        assert!((blast.player_speed.powi(2) - 324.0 * [0.5, 1.0, 2.0, 4.0][index]).abs() < 0.001);
    }
}

#[test]
fn edited_package_changes_new_shots_and_retains_in_flight_generation() {
    let fixture = Fixture::new();
    fixture.write(BOW);
    let mut host = PackageHost::new(&fixture.0);
    let old = host.fire(BowPower::Standard).unwrap();
    fixture.write(
        &BOW.replace("36.0", "48.0")
            .replace("6000.0", "9000.0")
            .replace("second 25", "second 10"),
    );
    poll_disk(&mut host);
    assert_eq!(host.status.state, PackageState::Reloading);
    assert_eq!(host.status.generation, 1);
    drain(&mut host);
    assert_eq!(host.status.state, PackageState::Loaded);
    assert_eq!(host.status.generation, 2);
    assert_eq!(host.shots_per_second(), 10);
    let new = host.fire(BowPower::Standard).unwrap();
    assert_eq!(new.projectile.speed, 48.0);
    assert_eq!(new.impact().energy, 9000.0);
    assert_eq!(old.projectile.speed, 36.0);
    assert_eq!(old.impact().energy, 6000.0);
    assert_eq!(old.generation(), 1);
}

#[test]
fn failed_reload_retains_last_good_package_and_recovers_after_save() {
    let fixture = Fixture::new();
    fixture.write(BOW);
    let mut host = PackageHost::new(&fixture.0);
    fixture.write("(define broken");
    poll_disk(&mut host);
    drain(&mut host);
    assert_eq!(host.status.state, PackageState::Error);
    assert!(host.status.error.is_some());
    assert_eq!(host.status.generation, 1);
    assert_eq!(
        host.fire(BowPower::Standard).unwrap().impact().energy,
        6000.0
    );
    let revision = host.revision();
    poll_disk(&mut host);
    assert_eq!(host.revision(), revision);
    fixture.write(BOW);
    poll_disk(&mut host);
    drain(&mut host);
    assert_eq!(host.status.state, PackageState::Loaded);
    assert!(host.status.error.is_none());
    assert_eq!(host.status.generation, 2);
}

#[test]
fn missing_initial_package_disables_bow_until_created() {
    let fixture = Fixture::new();
    let mut host = PackageHost::new(&fixture.0);
    assert_eq!(host.status.state, PackageState::Error);
    assert_eq!(host.shots_per_second(), 0);
    assert!(host.fire(BowPower::Low).is_err());
    fixture.write(BOW);
    poll_disk(&mut host);
    assert_eq!(host.status.state, PackageState::Loading);
    drain(&mut host);
    assert!(host.fire(BowPower::Low).is_ok());
}

#[test]
fn a_second_save_supersedes_an_unpublished_candidate() {
    let fixture = Fixture::new();
    fixture.write(BOW);
    let mut host = PackageHost::new(&fixture.0);
    fixture.write(&BOW.replace("36.0", "42.0"));
    poll_disk(&mut host);
    fixture.write(&BOW.replace("36.0", "50.0"));
    drain(&mut host);
    assert_eq!(
        host.fire(BowPower::Standard).unwrap().projectile.speed,
        50.0
    );
    assert_eq!(host.status.generation, 2);
}

#[test]
fn invalid_api_and_commands_are_rejected_before_installation() {
    for (from, to) in [
        ("version 1", "version 2"),
        ("second 25", "second 0"),
        ("36.0", "+nan.0"),
        ("180)", "180.5)"),
        ("6000.0", "-1.0"),
        ("0.35", "2.0"),
        ("(blast-for-power power)", "(missing-blast power)"),
    ] {
        assert!(
            compile(&BOW.replace(from, to)).is_err(),
            "accepted {from} -> {to}"
        );
    }
}

#[test]
fn runaway_callback_is_interrupted() {
    let source =
        format!("{BOW}\n(set! projectile-for-power (lambda (power) (let loop () (loop))))");
    let error = compile(&source).err().expect("loop was not interrupted");
    assert!(error.contains("budget"), "{error}");
}

#[test]
fn package_host_is_a_thread_safe_simulation_resource() {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<PackageHost>();
}

const TERRAIN: &str = include_str!("../../../packages/terrain/server.scm");

const CUSTOM_TERRAIN: &str = r#"
(define terrain-api-version 1)
(define terrain-version 7)
(define generator-identity "custom-test")
(define stone-slope 3.0)
(define terrain-height
  (tadd (constant 40.0) (tmul (fbm 0.02 3 2.0 0.5 1) (constant 8.0))))
(define terrain-temperature (constant 0.5))
(define terrain-moisture (constant 0.5))
(define terrain-soil (constant 0.5))
(define biomes
  (list (biome "sandbar" 'sand 'sand 2 -2.0 3.0 -2.0 3.0 -1000.0 5000.0)))
"#;

struct TerrainFixture(PathBuf);

impl TerrainFixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "jtech-terrain-pkg-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join(TERRAIN_PACKAGE)).unwrap();
        Self(root)
    }

    fn write(&self, source: &str) {
        fs::write(self.0.join(TERRAIN_PACKAGE).join("server.scm"), source).unwrap();
    }
}

impl Drop for TerrainFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn load_terrain_source(source: &str) -> Result<voxel_world::TerrainGenerator, String> {
    let fixture = TerrainFixture::new();
    fixture.write(source);
    load_terrain(&fixture.0)
}

#[test]
fn shipped_terrain_package_matches_the_native_default() {
    let loaded = load_terrain_source(TERRAIN).unwrap();
    let native = voxel_world::TerrainGenerator::default();
    assert_eq!(loaded.identity(), native.identity());
    assert_eq!(loaded.version(), native.version());
    assert_eq!(loaded.biome_count(), native.biome_count());
    assert_eq!(loaded.node_count(), native.node_count());
    for seed in [0u64, 3, 77] {
        for i in 0..64i64 {
            let x = i * 137 - 9000;
            let z = i * 211 + 7000;
            let a = loaded.sample(x, z, seed);
            let b = native.sample(x, z, seed);
            assert_eq!(a.height, b.height, "height mismatch at {x},{z},{seed}");
            assert_eq!(a.biome, b.biome, "biome mismatch at {x},{z},{seed}");
            assert_eq!(a.surface, b.surface);
            assert_eq!(a.subsurface, b.subsurface);
            assert_eq!(a.soil, b.soil);
            assert_eq!(a.temperature.to_bits(), b.temperature.to_bits());
            assert_eq!(a.moisture.to_bits(), b.moisture.to_bits());
        }
    }
    assert_eq!(
        loaded.column_bounds(-4, 6, 9),
        native.column_bounds(-4, 6, 9)
    );
}

#[test]
fn custom_scheme_graph_compiles_and_samples() {
    let generator = load_terrain_source(CUSTOM_TERRAIN).unwrap();
    assert_eq!(generator.identity(), "custom-test");
    assert_eq!(generator.version(), 7);
    assert_eq!(generator.biome_count(), 1);
    assert_eq!(generator.biome_name(voxel_world::Biome(0)), "sandbar");
    for i in 0..64i64 {
        let sample = generator.sample(i * 31 - 500, i * 17 + 90, 4);
        assert!(
            (40..=48).contains(&sample.height),
            "height {}",
            sample.height
        );
        assert_eq!(sample.surface, voxel_world::SAND);
        assert_eq!(sample.subsurface, voxel_world::SAND);
        assert_eq!(sample.soil, 1);
    }
    let chunk = voxel_world::Chunk::generate_with(glam::IVec3::new(0, 1, 0), 4, &generator);
    let surface = generator.sample(0, 0, 4).height - 32;
    assert_eq!(
        chunk.get(glam::IVec3::new(0, surface, 0)),
        voxel_world::SAND
    );
}

#[test]
fn invalid_terrain_packages_are_rejected() {
    for (from, to) in [
        (
            "(define terrain-api-version 1)",
            "(define terrain-api-version 2)",
        ),
        ("(define terrain-version 1)", "(define terrain-version 0)"),
        (
            "(biome \"shore\" 'sand 'sand 3",
            "(biome \"shore\" 'wood 'sand 3",
        ),
        ("(define biomes", "(define missing-biomes"),
        ("(define terrain-soil", "(define terrain-soil-typo"),
        ("(define stone-slope 4.0)", "(define stone-slope 0.0)"),
    ] {
        assert!(
            load_terrain_source(&TERRAIN.replace(from, to)).is_err(),
            "accepted {from} -> {to}"
        );
    }
    let non_finite = r#"
(define terrain-api-version 1)
(define terrain-version 1)
(define generator-identity "nan")
(define stone-slope 4.0)
(define terrain-height (tpow (constant -1.0) 0.5))
(define terrain-temperature (constant 0.5))
(define terrain-moisture (constant 0.5))
(define terrain-soil (constant 0.5))
(define biomes (list (biome "p" 'grass 'dirt 4 -2.0 3.0 -2.0 3.0 -1000.0 5000.0)))
"#;
    assert!(load_terrain_source(non_finite).is_err());
}

#[test]
fn runaway_terrain_package_is_interrupted() {
    let source = format!("{TERRAIN}\n(set! terrain-soil (let loop () (loop)))");
    let error = load_terrain_source(&source).expect_err("loop was not interrupted");
    assert!(error.contains("budget"), "{error}");
}

#[test]
fn terrain_generator_is_a_thread_safe_resource() {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<voxel_world::TerrainGenerator>();
}
