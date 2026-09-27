use std::fmt::Write;

use bevy::{prelude::*, text::LineBreak};
use protocol::{LauncherInfo, MeleeWeaponInfo, PackageState, PackageStatus};

use crate::{ClientSession, game_hud::palette};

/// Server package state is independent of movement prediction and local visuals.
pub(crate) struct ServerPackages {
    pub revision: Option<u64>,
    pub statuses: Vec<PackageStatus>,
    /// Launcher items replicated with the package set; drives firing rate,
    /// power presets and item display names without client-side duplication.
    pub launchers: Vec<LauncherInfo>,
    /// Melee weapons replicated with the package set; drives swing routing
    /// and item display names without client-side duplication.
    pub melee_weapons: Vec<MeleeWeaponInfo>,
}

impl Default for ServerPackages {
    fn default() -> Self {
        Self {
            revision: None,
            statuses: Vec::new(),
            launchers: Vec::new(),
            melee_weapons: Vec::new(),
        }
    }
}

impl ServerPackages {
    pub fn receive(
        &mut self,
        revision: u64,
        statuses: Vec<PackageStatus>,
        launchers: Vec<LauncherInfo>,
        melee_weapons: Vec<MeleeWeaponInfo>,
    ) {
        if self.revision.is_some_and(|current| revision <= current) {
            return;
        }
        self.revision = Some(revision);
        self.statuses = statuses;
        self.launchers = launchers;
        self.melee_weapons = melee_weapons;
    }

    /// Swing reach for the held item; unarmed reach when the item is not a
    /// replicated weapon.
    pub fn melee_range(&self, item: u32) -> f32 {
        self.melee_weapons
            .iter()
            .find(|weapon| weapon.item == item)
            .map_or(gameplay::combat::MELEE_HANDS.range, |weapon| {
                weapon.range
            })
    }

    /// Authored display name for a replicated weapon item.
    pub fn melee_name(&self, item: u32) -> Option<&str> {
        self.melee_weapons
            .iter()
            .find(|weapon| weapon.item == item)
            .map(|weapon| weapon.name.as_str())
    }

    /// Replicated info for a launcher item.
    pub fn launcher(&self, item: u32) -> Option<&LauncherInfo> {
        self.launchers.iter().find(|launcher| launcher.item == item)
    }

    /// True when the item is a replicated launcher.
    pub fn is_launcher(&self, item: u32) -> bool {
        self.launcher(item).is_some()
    }

    /// Authored display name for a replicated launcher item.
    pub fn launcher_name(&self, item: u32) -> Option<&str> {
        self.launcher(item).map(|launcher| launcher.name.as_str())
    }

    /// Display label for a launcher's power preset index, clamped to the
    /// authored list. An empty `powers` list means unshootable.
    pub fn launcher_power_label(launcher: &LauncherInfo, power: u8) -> Option<&str> {
        launcher
            .powers
            .get(usize::from(power).min(launcher.powers.len().saturating_sub(1)))
            .map(String::as_str)
    }

    /// Unique non-stacking items (launchers and weapons); everything else is a stack.
    pub fn is_equipment(&self, item: u32) -> bool {
        self.is_launcher(item)
            || self
                .melee_weapons
                .iter()
                .any(|weapon| weapon.item == item && weapon.kind == protocol::ItemKind::Equipment)
    }
}

#[derive(Component)]
pub(crate) struct PackagePanel;

pub(crate) fn spawn(commands: &mut Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 13.0,
            ..default()
        },
        TextLayout::new_with_linebreak(LineBreak::WordOrCharacter),
        TextColor(palette::IVORY),
        TextShadow::default(),
        Node {
            position_type: PositionType::Absolute,
            top: px(16),
            right: px(18),
            width: px(340),
            max_width: percent(40),
            padding: UiRect::all(px(11)),
            border: UiRect::all(px(1)),
            display: Display::None,
            ..default()
        },
        BackgroundColor(palette::PANEL),
        BorderColor::all(palette::BORDER),
        BorderRadius::all(px(6)),
        GlobalZIndex(20),
        PackagePanel,
    ));
}

pub(crate) fn update(
    session: Res<ClientSession>,
    mut panel: Single<(&mut Text, &mut TextColor, &mut Node), With<PackagePanel>>,
    mut displayed_revision: Local<Option<(u64, u64)>>,
) {
    let packages = &session.packages;
    let revision = packages
        .revision
        .map(|revision| (session.session, revision));
    if *displayed_revision == revision {
        return;
    }
    *displayed_revision = revision;
    let (text, color, node) = &mut *panel;
    node.display = if packages.revision.is_some() {
        Display::Flex
    } else {
        Display::None
    };
    text.0 = panel_text(packages);
    color.0 = if packages
        .statuses
        .iter()
        .any(|package| matches!(package.state, PackageState::Error))
    {
        palette::DANGER
    } else if packages.statuses.iter().any(|package| {
        matches!(
            package.state,
            PackageState::Loading | PackageState::Reloading
        )
    }) {
        palette::CONNECTING
    } else {
        palette::IVORY
    };
}

fn panel_text(packages: &ServerPackages) -> String {
    let mut text = String::from("SERVER PACKAGES");
    if packages.statuses.is_empty() {
        text.push_str("\nNo packages loaded");
    }
    for package in &packages.statuses {
        let state = match package.state {
            PackageState::Loading => "loading",
            PackageState::Reloading => "reloading",
            PackageState::Loaded => "loaded",
            PackageState::Error => "error",
        };
        let _ = write!(text, "\n{}: {state}", package.id);
        if package.generation == 0 {
            text.push_str(" (inactive)");
        } else if matches!(package.state, PackageState::Loaded) {
            let _ = write!(text, " (v{})", package.generation);
        } else {
            let _ = write!(text, "\nPrevious v{} active", package.generation);
        }
        if let Some(error) = &package.error {
            let _ = write!(text, "\n{error}");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(generation: u64, state: PackageState) -> PackageStatus {
        PackageStatus {
            id: "explosive-bow".into(),
            generation,
            state,
            error: None,
        }
    }

    fn launcher() -> LauncherInfo {
        LauncherInfo {
            item: protocol::FIRST_PACKAGE_ITEM,
            package: "explosive-bow".into(),
            name: "Explosive Bow".into(),
            powers: vec!["Low".into(), "Standard".into(), "High".into()],
            shots_per_second: 25,
        }
    }

    #[test]
    fn stale_package_messages_cannot_restore_old_state_or_item_tables() {
        let mut packages = ServerPackages::default();
        packages.receive(0, vec![status(0, PackageState::Loading)], vec![], vec![]);
        assert_eq!(packages.revision, Some(0));
        packages.receive(2, vec![status(1, PackageState::Loaded)], vec![launcher()], vec![]);
        for revision in [0, 1, 2] {
            packages.receive(revision, vec![status(0, PackageState::Error)], vec![], vec![]);
        }
        assert_eq!(packages.revision, Some(2));
        assert_eq!(packages.launchers.len(), 1);
        assert_eq!(packages.statuses[0].generation, 1);
        assert!(matches!(packages.statuses[0].state, PackageState::Loaded));
    }

    #[test]
    fn panel_follows_package_lifecycle_and_disconnect() {
        let mut app = App::new();
        app.init_resource::<ClientSession>()
            .add_systems(Startup, |mut commands: Commands| spawn(&mut commands))
            .add_systems(Update, update);
        app.update();
        let panel = app
            .world_mut()
            .query_filtered::<Entity, With<PackagePanel>>()
            .single(app.world())
            .unwrap();
        assert_eq!(
            app.world().get::<Node>(panel).unwrap().display,
            Display::None
        );
        for (revision, generation, state) in [
            (0, 0, PackageState::Loading),
            (1, 1, PackageState::Loaded),
            (2, 1, PackageState::Reloading),
            (3, 1, PackageState::Error),
        ] {
            let mut package = status(generation, state);
            if matches!(package.state, PackageState::Error) {
                package.error = Some("unbound identifier: explode!".into());
            }
            app.world_mut()
                .resource_mut::<ClientSession>()
                .packages
                .receive(revision, vec![package], vec![], vec![]);
            app.update();
            assert_eq!(
                app.world().get::<Node>(panel).unwrap().display,
                Display::Flex
            );
        }
        // Error details belong to the failed replacement, not the still-active code.
        assert_eq!(
            app.world().resource::<ClientSession>().packages.statuses[0].generation,
            1
        );
        app.world_mut()
            .resource_mut::<ClientSession>()
            .disconnect("test");
        app.update();
        assert_eq!(
            app.world().get::<Node>(panel).unwrap().display,
            Display::None
        );
        let packages = &app.world().resource::<ClientSession>().packages;
        assert_eq!(packages.revision, None);
        assert!(packages.statuses.is_empty());
        assert!(packages.launchers.is_empty());
    }
}
