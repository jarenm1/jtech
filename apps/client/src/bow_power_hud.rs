use bevy::{prelude::*, window::CursorOptions};
use protocol::EXPLOSIVE_BOW_ITEM;

use crate::{
    ClientSession,
    game_hud::{GameplayHud, palette},
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
                Text::new("BOW POWER"),
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
    cursor: Option<Single<&CursorOptions>>,
    mut session: ResMut<ClientSession>,
) {
    if session.health.is_depleted() {
        return;
    }
    // Headless has no window entity, so no `CursorOptions`: no clicks arrive.
    let cursor_visible = cursor.is_some_and(|cursor| cursor.visible);
    if cursor_visible && session.held_item() == EXPLOSIVE_BOW_ITEM {
        for interaction in &buttons {
            if *interaction == Interaction::Pressed {
                session.bow_power = session.bow_power.next();
            }
        }
    }
}

pub(crate) fn update(
    session: Res<ClientSession>,
    cursor: Option<Single<&CursorOptions>>,
    mut button: Single<(&mut Node, &Interaction, &mut BackgroundColor), With<PowerButton>>,
    mut label: Single<&mut Text, With<PowerLabel>>,
    mut shown: Local<Option<(bool, &'static str, Color)>>,
) {
    // Headless has no window entity, so no `CursorOptions`: no hover or press.
    let cursor_visible = cursor.is_some_and(|cursor| cursor.visible);
    let (node, interaction, color) = &mut *button;
    let visible = session.held_item() == EXPLOSIVE_BOW_ITEM;
    let text = session.bow_power.label();
    let tint = match **interaction {
        Interaction::Pressed if cursor_visible => palette::PRESSED,
        Interaction::Hovered if cursor_visible => palette::HOVER,
        _ => palette::PANEL,
    };
    // The panel is static between input changes; skip the per-frame `Text`
    // allocation and component writes when nothing it renders has moved.
    if *shown == Some((visible, text, tint)) {
        return;
    }
    *shown = Some((visible, text, tint));
    node.display = if visible {
        Display::Flex
    } else {
        Display::None
    };
    color.0 = tint;
    **label = Text::new(text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::BowPower;

    #[test]
    fn clicks_cycle_once_only_with_bow_and_released_cursor() {
        let mut app = App::new();
        app.insert_resource(ClientSession {
            selected: 6,
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
