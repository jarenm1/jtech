//! Reliable package status replication, independent of world snapshot cadence.
use super::Simulation;
use protocol::ServerMessage;

impl Simulation {
    pub(super) fn poll_packages(&mut self) {
        if self.packages.poll() {
            for player in self.players.values_mut() {
                player.next_bow_time = 0;
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
        let message = ServerMessage::Packages {
            revision,
            packages: vec![self.packages.status().clone()],
            bow_shots_per_second: self.packages.shots_per_second(),
        };
        for id in recipients {
            if self.send(id, &message) {
                self.players.get_mut(&id).unwrap().package_revision = Some(revision);
            }
        }
    }
}

#[cfg(test)]
pub(super) fn test_blast(power: protocol::BowPower) -> game_packages::BlastSpec {
    test_package().impact(power)
}

#[cfg(test)]
pub(super) fn test_package() -> &'static game_packages::BowPackage {
    static PACKAGE: std::sync::OnceLock<game_packages::BowPackage> = std::sync::OnceLock::new();
    PACKAGE.get_or_init(|| {
        game_packages::BowPackage::compile(
            include_str!("../../../packages/explosive-bow/server.scm").to_owned(),
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../packages/explosive-bow/server.scm"),
            1,
        )
        .unwrap()
    })
}
