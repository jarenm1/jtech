use std::fmt::Write;

use bevy::{prelude::*, text::LineBreak};
use protocol::{PackageState, PackageStatus};

use crate::ClientSession;

/// Server package state is independent of movement prediction and local visuals.
pub(crate) struct ServerPackages {
    pub revision: Option<u64>,
    pub statuses: Vec<PackageStatus>,
    pub bow_shots_per_second: u32,
}

impl Default for ServerPackages {
    fn default() -> Self {
        Self {
            revision: None,
            statuses: Vec::new(),
            bow_shots_per_second: protocol::EXPLOSIVE_BOW_SHOTS_PER_SECOND,
        }
    }
}

impl ServerPackages {
    pub fn receive(
        &mut self,
        revision: u64,
        statuses: Vec<PackageStatus>,
        bow_shots_per_second: u32,
    ) {
        if self.revision.is_some_and(|current| revision <= current) {
            return;
        }
        self.revision = Some(revision);
        self.statuses = statuses;
        // Zero disables firing when no bow package is active.
        self.bow_shots_per_second = bow_shots_per_second;
    }
}

#[derive(Component)]
pub(crate) struct PackagePanel;

pub(crate) fn spawn(commands: &mut Commands) {
    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 14.0,
            ..default()
        },
        TextLayout::new_with_linebreak(LineBreak::WordOrCharacter),
        TextColor(Color::srgb(0.78, 0.92, 0.8)),
        TextShadow::default(),
        Node {
            position_type: PositionType::Absolute,
            top: px(18),
            right: px(20),
            width: px(360),
            max_width: percent(38),
            padding: UiRect::all(px(12)),
            display: Display::None,
            ..default()
        },
        BackgroundColor(Color::srgba(0.025, 0.04, 0.07, 0.9)),
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
        Color::srgb(1.0, 0.63, 0.55)
    } else if packages.statuses.iter().any(|package| {
        matches!(
            package.state,
            PackageState::Loading | PackageState::Reloading
        )
    }) {
        Color::srgb(1.0, 0.86, 0.5)
    } else {
        Color::srgb(0.78, 0.92, 0.8)
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

    #[test]
    fn stale_package_messages_cannot_restore_old_state_or_firing_rate() {
        let mut packages = ServerPackages::default();
        packages.receive(0, vec![status(0, PackageState::Loading)], 0);
        assert_eq!(packages.revision, Some(0));
        packages.receive(2, vec![status(1, PackageState::Loaded)], 10);
        for revision in [0, 1, 2] {
            packages.receive(revision, vec![status(0, PackageState::Error)], 25);
        }
        assert_eq!(packages.revision, Some(2));
        assert_eq!(packages.bow_shots_per_second, 10);
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
                .receive(revision, vec![package], 10);
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
        assert_eq!(
            packages.bow_shots_per_second,
            protocol::EXPLOSIVE_BOW_SHOTS_PER_SECOND
        );
    }
}
