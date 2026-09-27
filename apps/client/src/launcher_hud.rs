use bevy::{prelude::*, window::CursorOptions};

use crate::{
    ClientSession,
    game_hud::{GameplayHud, palette},
    package_hud::ServerPackages,
};

#[derive(Component)]
pub(crate) struct PowerButton;

#[derive(Component)]
pub(crate) struct PowerLabel;

pub(crate) fn spawn(commands: &mut Commands) {
    commands
        .spawn((
            Button,
            Node {
                position_type: PositionType::Absolute,
                bottom: px(22),
                right: px(24),
                padding: UiRect::axes(px(14), px(9)),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                row_gap: px(2),
                border: UiRect::all(px(2)),
                display: Display::None,
                ..default()
            },
            BackgroundColor(palette::PANEL),
            BorderColor::all(palette::AMBER),
            BorderRadius::all(px(6)),
            GlobalZIndex(10),
            GameplayHud,
            PowerButton,
        ))
        .with_children(|button| {
            button.spawn((
                Text::new("POWER"),
                TextFont {
                    font_size: 11.0,
                    ..default()
                },
                TextColor(palette::MUTED),
                TextShadow::default(),
            ));
            button.spawn((
                Text::new(""),
                TextFont {
                    font_size: 18.0,
                    ..default()
                },
                TextColor(palette::AMBER),
                TextShadow::default(),
                PowerLabel,
            ));
        });
}

pub(crate) fn cycle_on_click(
    buttons: Query<&Interaction, (With<PowerButton>, Changed<Interaction>)>,
    cursor: Single<&CursorOptions>,
    mut session: ResMut<ClientSession>,
) {
    if session.health.is_depleted() {
        return;
    }
    if cursor.visible && session.packages.is_launcher(session.held_item()) {
        for interaction in &buttons {
            if *interaction == Interaction::Pressed {
                session.cycle_launch_power();
            }
        }
    }
}

pub(crate) fn update(
    session: Res<ClientSession>,
    cursor: Single<&CursorOptions>,
    mut button: Single<(&mut Node, &Interaction, &mut BackgroundColor), With<PowerButton>>,
    mut label: Single<&mut Text, With<PowerLabel>>,
) {
    let (node, interaction, color) = &mut *button;
    let launcher = session.packages.launcher(session.held_item());
    node.display = if launcher.is_some() {
        Display::Flex
    } else {
        Display::None
    };
    color.0 = match **interaction {
        Interaction::Pressed if cursor.visible => palette::PRESSED,
        Interaction::Hovered if cursor.visible => palette::HOVER,
        _ => palette::PANEL,
    };
    **label = Text::new(
        launcher
            .and_then(|launcher| {
                ServerPackages::launcher_power_label(launcher, session.launch_power)
            })
            .unwrap_or(""),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::LauncherInfo;

    fn launcher() -> LauncherInfo {
        LauncherInfo {
            item: protocol::FIRST_PACKAGE_ITEM,
            package: "explosive-bow".into(),
            name: "Explosive Bow".into(),
            powers: vec![
                "Low".into(),
                "Standard".into(),
                "High".into(),
                "Extreme".into(),
            ],
            shots_per_second: 25,
        }
    }

    #[test]
    fn clicks_cycle_once_only_with_launcher_and_released_cursor() {
        let mut app = App::new();
        app.insert_resource(ClientSession {
            selected: 6,
            ..default()
        })
        .add_systems(Startup, |mut commands: Commands| spawn(&mut commands))
        .add_systems(Update, (cycle_on_click, update).chain());
        app.world_mut()
            .resource_mut::<ClientSession>()
            .packages
            .launchers = vec![launcher()];
        app.world_mut().resource_mut::<ClientSession>().hotbar[5] =
            Some(protocol::FIRST_PACKAGE_ITEM);
        let cursor = app.world_mut().spawn(CursorOptions::default()).id();
        app.update();
        let button = app
            .world_mut()
            .query_filtered::<Entity, With<PowerButton>>()
            .single(app.world())
            .unwrap();
        assert_eq!(
            app.world().entity(button).get::<Node>().unwrap().display,
            Display::Flex
        );
        for expected in [1, 2, 3, 0] {
            *app.world_mut()
                .entity_mut(button)
                .get_mut::<Interaction>()
                .unwrap() = Interaction::Pressed;
            app.update();
            assert_eq!(
                app.world().resource::<ClientSession>().launch_power,
                expected
            );
            app.update();
            app.update();
            assert_eq!(
                app.world().resource::<ClientSession>().launch_power,
                expected
            );
            *app.world_mut()
                .entity_mut(button)
                .get_mut::<Interaction>()
                .unwrap() = Interaction::Hovered;
            app.update();
            assert_eq!(
                app.world().resource::<ClientSession>().launch_power,
                expected
            );
        }
        app.world_mut()
            .entity_mut(cursor)
            .get_mut::<CursorOptions>()
            .unwrap()
            .visible = false;
        *app.world_mut()
            .entity_mut(button)
            .get_mut::<Interaction>()
            .unwrap() = Interaction::Pressed;
        app.update();
        assert_eq!(
            app.world().resource::<ClientSession>().launch_power,
            0
        );
        app.world_mut()
            .entity_mut(cursor)
            .get_mut::<CursorOptions>()
            .unwrap()
            .visible = true;
        app.update();
        assert_eq!(
            app.world().resource::<ClientSession>().launch_power,
            0
        );
        app.world_mut().resource_mut::<ClientSession>().selected = 3;
        *app.world_mut()
            .entity_mut(button)
            .get_mut::<Interaction>()
            .unwrap() = Interaction::None;
        app.update();
        assert_eq!(
            app.world().entity(button).get::<Node>().unwrap().display,
            Display::None
        );
        *app.world_mut()
            .entity_mut(button)
            .get_mut::<Interaction>()
            .unwrap() = Interaction::Pressed;
        app.update();
        assert_eq!(
            app.world().resource::<ClientSession>().launch_power,
            0
        );
    }
}
