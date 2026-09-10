use bevy::{prelude::*, window::CursorOptions};
use protocol::EXPLOSIVE_BOW_SLOT;

use crate::ClientSession;

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
                bottom: px(112),
                left: px(260),
                padding: UiRect::all(px(12)),
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(0.025, 0.04, 0.07, 0.9)),
            PowerButton,
        ))
        .with_children(|button| {
            button.spawn((
                Text::new(""),
                TextFont {
                    font_size: 18.0,
                    ..default()
                },
                TextColor(Color::srgb(0.94, 0.97, 1.0)),
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
    if cursor.visible && session.selected == EXPLOSIVE_BOW_SLOT {
        for interaction in &buttons {
            if *interaction == Interaction::Pressed {
                session.bow_power = session.bow_power.next();
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
    node.display = if session.selected == EXPLOSIVE_BOW_SLOT {
        Display::Flex
    } else {
        Display::None
    };
    color.0 = match **interaction {
        Interaction::Pressed if cursor.visible => Color::srgb(0.3, 0.35, 0.5),
        Interaction::Hovered if cursor.visible => Color::srgb(0.15, 0.2, 0.3),
        _ => Color::srgba(0.025, 0.04, 0.07, 0.9),
    };
    **label = Text::new(format!(
        "Bow power: {}  |  R cycle  |  Esc to click",
        session.bow_power.label()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::BowPower;

    #[test]
    fn clicks_cycle_once_only_with_bow_and_released_cursor() {
        let mut app = App::new();
        app.insert_resource(ClientSession {
            selected: EXPLOSIVE_BOW_SLOT,
            ..default()
        })
        .add_systems(Startup, |mut commands: Commands| spawn(&mut commands))
        .add_systems(Update, (cycle_on_click, update).chain());
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
        for expected in [
            BowPower::High,
            BowPower::Extreme,
            BowPower::Low,
            BowPower::Standard,
        ] {
            *app.world_mut()
                .entity_mut(button)
                .get_mut::<Interaction>()
                .unwrap() = Interaction::Pressed;
            app.update();
            assert_eq!(app.world().resource::<ClientSession>().bow_power, expected);
            app.update();
            app.update();
            assert_eq!(app.world().resource::<ClientSession>().bow_power, expected);
            *app.world_mut()
                .entity_mut(button)
                .get_mut::<Interaction>()
                .unwrap() = Interaction::Hovered;
            app.update();
            assert_eq!(app.world().resource::<ClientSession>().bow_power, expected);
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
            app.world().resource::<ClientSession>().bow_power,
            BowPower::Standard
        );
        app.world_mut()
            .entity_mut(cursor)
            .get_mut::<CursorOptions>()
            .unwrap()
            .visible = true;
        app.update();
        assert_eq!(
            app.world().resource::<ClientSession>().bow_power,
            BowPower::Standard
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
            app.world().resource::<ClientSession>().bow_power,
            BowPower::Standard
        );
    }
}
