//! Bow draw charge bar: fills as the explosive bow charges, hidden otherwise.
use bevy::prelude::*;

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

/// Ticks a ranged weapon draws before firing; matches the server's wind-up.
const DRAW_TICKS: f32 = 60.0;

pub(crate) fn spawn(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: percent(50),
                top: percent(58),
                margin: UiRect::left(px(-120)),
                width: px(240),
                height: px(14),
                border: UiRect::all(px(2)),
                display: Display::None,
                ..default()
            },
            BackgroundColor(palette::PANEL),
            BorderColor::all(palette::AMBER),
            BorderRadius::all(px(7)),
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
                BorderRadius::all(px(5)),
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
    let visible = crate::held_item::draws(&session, session.held_item());
    let fraction = (f32::from(session.state.charge) / DRAW_TICKS).clamp(0.0, 1.0);
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
    use protocol::{ItemKind, MeleeWeaponInfo};

    /// A replicated weapon; only a ranged one draws and fills the charge bar.
    fn weapon(item: u32, name: &str, attack_kind: controller::BasicAttackKind) -> MeleeWeaponInfo {
        MeleeWeaponInfo {
            item,
            package: "melee".into(),
            name: name.into(),
            kind: ItemKind::Equipment,
            range: 16.0,
            damage: 14,
            cooldown_ticks: 30,
            knockback: 0.0,
            attack_kind,
            charge_ticks: if attack_kind == controller::BasicAttackKind::Ranged {
                protocol::DRAW_TICKS
            } else {
                0
            },
            model: None,
        }
    }

    #[test]
    fn bar_fills_with_the_draw_and_hides_without_a_ranged_weapon() {
        let mut app = App::new();
        let mut session = ClientSession {
            selected: 6,
            ..default()
        };
        // Slot 6 holds the ranged bow (item 8); slot 3 holds the melee sword (7).
        session.hotbar[5] = Some(8);
        session.hotbar[2] = Some(7);
        session.packages.melee_weapons = vec![
            weapon(7, "Sword", controller::BasicAttackKind::Melee),
            weapon(8, "Bow", controller::BasicAttackKind::Ranged),
        ];
        app.insert_resource(session)
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
