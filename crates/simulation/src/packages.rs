//! Reliable package status replication, independent of world snapshot cadence.
use super::{AssetDownload, Simulation};
use protocol::ServerMessage;

impl Simulation {
    pub(super) fn poll_packages(&mut self) {
        if self.packages.poll() {
            for player in self.players.values_mut() {
                player.next_launch.clear();
            }
        }
    }

    pub(super) fn replicate_packages(&mut self) {
        let revision = self.packages.revision();
        let recipients: Vec<_> = self
            .players
            .iter()
            .filter(|(_, player)| player.package_revision != Some(revision))
            .map(|(&id, _)| id)
            .collect();
        if recipients.is_empty() {
            return;
        }
        let mut packages = self.packages.statuses();
        packages.push(protocol::PackageStatus {
            id: "terrain".into(),
            generation: 1,
            state: protocol::PackageState::Loaded,
            error: None,
        });
        let message = ServerMessage::Packages {
            revision,
            packages,
            launchers: {
                let mut launchers: Vec<_> = self
                    .packages
                    .launcher_table()
                    .launchers()
                    .map(|entry| protocol::LauncherInfo {
                        item: entry.launcher.item,
                        package: entry.package.clone(),
                        name: entry.launcher.name.clone(),
                        powers: entry
                            .launcher
                            .powers()
                            .iter()
                            .map(|power| power.label.clone())
                            .collect(),
                        shots_per_second: entry.launcher.shots_per_second,
                    })
                    .collect();
                launchers.sort_by_key(|launcher| launcher.item);
                launchers
            },
            melee_weapons: {
                let mut weapons: Vec<_> = self
                    .packages
                    .melee_table()
                    .weapons()
                    .map(|weapon| protocol::MeleeWeaponInfo {
                        item: weapon.id,
                        package: weapon.package.clone(),
                        name: weapon.name.clone(),
                        kind: protocol::ItemKind::Equipment,
                        range: weapon.spec.range,
                        damage: weapon.spec.damage,
                        cooldown_ticks: weapon.spec.cooldown_ticks,
                        knockback: weapon.spec.knockback,
                        model: weapon.model.clone(),
                    })
                    .collect();
                weapons.sort_by_key(|weapon| weapon.item);
                weapons
            },
            assets: self.packages.asset_manifest(),
        };
        for id in recipients {
            if self.send(id, &message) {
                self.players.get_mut(&id).unwrap().package_revision = Some(revision);
            }
        }
    }

    /// Queue one package asset for paced transmission. Bursts would overflow the
    /// reliable send queue and disconnect the client, so `advance_assets` emits
    /// a bounded number of chunks per tick instead.
    pub(super) fn request_asset(&mut self, id: u64, package: &str, path: &str) {
        let bytes = match self.packages.read_asset(package, path) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("asset {package}/{path} request from {id}: {error}");
                return;
            }
        };
        if let Some(player) = self.players.get_mut(&id) {
            player.asset_queue.push_back(AssetDownload {
                package: package.to_string(),
                path: path.to_string(),
                bytes: std::sync::Arc::new(bytes),
                offset: 0,
            });
        }
    }

    /// Emit a bounded slice of each player's asset queue every tick. Chunks are
    /// reliable and ordered, so `offset` strictly advances until the file ends.
    pub(super) fn advance_assets(&mut self) {
        const CHUNK: usize = 64 * 1024;
        const TICK_BUDGET: usize = 4;
        let ids: Vec<u64> = self.players.keys().copied().collect();
        for id in ids {
            for _ in 0..TICK_BUDGET {
                let next = self
                    .players
                    .get(&id)
                    .and_then(|player| player.asset_queue.front().map(|d| {
                        let end = (d.offset as usize + CHUNK).min(d.bytes.len());
                        (
                            d.package.clone(),
                            d.path.clone(),
                            d.offset,
                            d.bytes.len() as u32,
                            d.bytes[d.offset as usize..end].to_vec(),
                        )
                    }));
                let Some((package, path, offset, total, data)) = next else {
                    break;
                };
                let sent = self.send(
                    id,
                    &ServerMessage::AssetData {
                        package: package.clone(),
                        path: path.clone(),
                        offset,
                        total,
                        data,
                    },
                );
                if !sent {
                    break;
                }
                let Some(player) = self.players.get_mut(&id) else {
                    break;
                };
                let Some(download) = player.asset_queue.front_mut() else {
                    break;
                };
                download.offset += CHUNK as u32;
                if download.offset as usize >= download.bytes.len() {
                    player.asset_queue.pop_front();
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) fn test_blast(power: u8) -> game_packages::BlastSpec {
    test_package().powers()[usize::from(power)].blast
}

#[cfg(test)]
pub(super) fn test_shot(power: u8) -> game_packages::Shot {
    test_package().shot(power).unwrap()
}

#[cfg(test)]
pub(super) fn test_package() -> std::sync::Arc<game_packages::LauncherPackage> {
    static PACKAGE: std::sync::LazyLock<std::sync::Arc<game_packages::LauncherPackage>> =
        std::sync::LazyLock::new(|| {
            std::sync::Arc::new(
                game_packages::LauncherPackage::compile(
                    include_str!("../../../packages/explosive-bow/server.scm").to_owned(),
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../packages/explosive-bow/server.scm"),
                    1,
                )
                .unwrap(),
            )
        });
    PACKAGE.clone()
}
