//! Bow draw charge bar: fills as the explosive bow charges, hidden otherwise.
use bevy::prelude::*;
use protocol::EXPLOSIVE_BOW_ITEM;

use crate::{
    ClientSession,
    game_hud::{GameplayHud, palette},
};

/// Outer track of the charge bar.
#[derive(Component)]
pub(crate) struct ChargeTrack;

/// Inner fill whose width tracks the draw fraction.
#[derive(Component)]
pub(crate) struct ChargeFill;

pub(crate) fn spawn(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: px(22),
                right: px(24),
                width: px(180),
                height: px(12),
                border: UiRect::all(px(2)),
                display: Display::None,
                ..default()
            },
            BackgroundColor(palette::PANEL),
            BorderColor::all(palette::AMBER),
            BorderRadius::all(px(6)),
            GlobalZIndex(10),
            GameplayHud,
            ChargeTrack,
        ))
        .with_children(|track| {
            track.spawn((
                Node {
                    width: percent(0.0),
                    height: percent(100.0),
                    ..default()
                },
                BackgroundColor(palette::AMBER),
                BorderRadius::all(px(4)),
                ChargeFill,
            ));
        });
}

pub(crate) fn update(
    session: Res<ClientSession>,
    mut track: Single<&mut Node, (With<ChargeTrack>, Without<ChargeFill>)>,
    mut fill: Single<&mut Node, (With<ChargeFill>, Without<ChargeTrack>)>,
    mut shown: Local<Option<(bool, u32)>>,
) {
    let visible = session.held_item() == EXPLOSIVE_BOW_ITEM;
    let fraction = controller::MovementProfile::default().charge_fraction(session.state.charge);
    // The bar is static between charge changes; skip the per-frame writes.
    let permille = (fraction * 1000.0).round() as u32;
    if *shown == Some((visible, permille)) {
        return;
    }
    *shown = Some((visible, permille));
    track.display = if visible {
        Display::Flex
    } else {
        Display::None
    };
    fill.width = percent(fraction * 100.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_fills_with_the_draw_and_hides_without_the_bow() {
        let mut app = App::new();
        app.insert_resource(ClientSession {
            selected: 6,
            ..default()
        })
        .add_systems(Startup, |mut commands: Commands| spawn(&mut commands))
        .add_systems(Update, update);
        app.update();
        let track = app
            .world_mut()
            .query_filtered::<Entity, With<ChargeTrack>>()
            .single(app.world())
            .unwrap();
        let fill = app
            .world_mut()
            .query_filtered::<Entity, With<ChargeFill>>()
            .single(app.world())
            .unwrap();
        assert_eq!(
            app.world().entity(track).get::<Node>().unwrap().display,
            Display::Flex
        );
        assert_eq!(
            app.world().entity(fill).get::<Node>().unwrap().width,
            percent(0.0)
        );
        app.world_mut().resource_mut::<ClientSession>().state.charge = 30;
        app.update();
        assert_eq!(
            app.world().entity(fill).get::<Node>().unwrap().width,
            percent(50.0)
        );
        app.world_mut().resource_mut::<ClientSession>().selected = 3;
        app.update();
        assert_eq!(
            app.world().entity(track).get::<Node>().unwrap().display,
            Display::None
        );
    }
}
