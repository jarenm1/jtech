//! Shared chrome for the floating inventory pane: a centered frame, a floating
//! close button, and drag-to-move from the pane background.
//!
//! The pane is positioned from the window center plus a [`PaneOffset`], so it
//! stays centered on resize and moves when its background is dragged.

use bevy::{prelude::*, ui::FocusPolicy};

use crate::{
    inventory_ui::InventoryUi,
    pause_menu::PauseMenu,
    ui_theme::palette,
};

/// A draggable floating pane. Its background doubles as the drag handle.
#[derive(Component)]
pub(crate) struct Pane {
    pub size: Vec2,
}

/// A pane's close button.
#[derive(Component)]
pub(crate) struct PaneClose;

/// Offset from the window center, in logical pixels.
#[derive(Component, Default)]
pub(crate) struct PaneOffset(pub Vec2);

/// Root components for a pane of `size`, centered in the window and hidden
/// until its toggle opens it. The extra top padding leaves room for the close
/// button above the content.
pub(crate) fn root(size: Vec2) -> impl Bundle {
    (
        Node {
            position_type: PositionType::Absolute,
            left: percent(50),
            top: percent(50),
            width: px(size.x),
            height: px(size.y),
            margin: UiRect {
                left: px(-size.x / 2.0),
                top: px(-size.y / 2.0),
                ..default()
            },
            flex_direction: FlexDirection::Column,
            padding: UiRect::ZERO,
            display: Display::None,
            ..default()
        },
        // No backdrop: the pane is just the book, so the world shows around it.
        GlobalZIndex(90),
        FocusPolicy::Block,
        // Track hover so hotbar drags can unbind anywhere over the pane, and
        // press to drag the pane itself.
        Interaction::None,
        Pane { size },
        PaneOffset::default(),
    )
}

/// Spawn the floating close button at the pane's top right.
pub(crate) fn spawn_close_button(pane: &mut ChildSpawnerCommands) {
    pane.spawn((
        Button,
        Node {
            position_type: PositionType::Absolute,
            top: px(5),
            right: px(6),
            width: px(16),
            height: px(16),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            ..default()
        },
        BackgroundColor(Color::NONE),
        BorderRadius::all(px(palette::INNER_RADIUS)),
        PaneClose,
    ))
    .with_children(|close| {
        close.spawn((
            Text::new("X"),
            TextFont {
                font_size: 11.0,
                ..default()
            },
            TextColor(palette::TEXT_MUTED),
        ));
    });
}

/// Close the pane from its button, tinting the button on hover.
pub(crate) fn close(
    mut buttons: Query<(&Interaction, &PaneClose, &mut BackgroundColor)>,
    mut inventory: ResMut<InventoryUi>,
    mut menu: ResMut<PauseMenu>,
) {
    for (interaction, _, mut color) in &mut buttons {
        let desired = match interaction {
            Interaction::Pressed => palette::SLOT_PRESSED,
            Interaction::Hovered => palette::SLOT_HOVER,
            Interaction::None => Color::NONE,
        };
        if color.0 != desired {
            color.0 = desired;
        }
        if *interaction != Interaction::Pressed {
            continue;
        }
        inventory.open = false;
        // The click must not reach gameplay the frame the pane closes.
        menu.hold_for_mouse_release();
    }
}

/// Reposition the pane while its background is dragged.
pub(crate) fn drag(
    buttons: Res<ButtonInput<MouseButton>>,
    window: Single<&Window>,
    mut panes: Query<(&Interaction, &Pane, &mut PaneOffset, &mut Node)>,
    mut drag: Local<Option<(Vec2, Vec2)>>,
) {
    let Some(cursor) = window.cursor_position() else {
        *drag = None;
        return;
    };
    for (interaction, _, offset, _) in &panes {
        if *interaction == Interaction::Pressed {
            *drag = Some((cursor, offset.0));
        }
    }
    let Some((start_cursor, start_offset)) = *drag else {
        return;
    };
    if !buttons.pressed(MouseButton::Left) {
        *drag = None;
        return;
    }
    for (_, pane, mut offset, mut node) in &mut panes {
        offset.0 = start_offset + (cursor - start_cursor);
        node.margin.left = px(-pane.size.x / 2.0 + offset.0.x);
        node.margin.top = px(-pane.size.y / 2.0 + offset.0.y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_button_hides_the_pane_and_holds_the_mouse() {
        let mut app = App::new();
        app.init_resource::<InventoryUi>()
            .init_resource::<PauseMenu>()
            .add_systems(Startup, |mut commands: Commands| {
                let pane = commands.spawn(root(Vec2::new(640.0, 220.0))).id();
                commands.entity(pane).with_children(spawn_close_button);
            })
            .add_systems(Update, close);
        app.update();
        app.world_mut().resource_mut::<InventoryUi>().open = true;
        let button = app
            .world_mut()
            .query_filtered::<Entity, With<PaneClose>>()
            .single(app.world())
            .unwrap();
        *app.world_mut().get_mut::<Interaction>(button).unwrap() = Interaction::Pressed;
        app.update();
        assert!(!app.world().resource::<InventoryUi>().open);
        // The closing click must not reach gameplay.
        assert!(app.world().resource::<PauseMenu>().blocks_gameplay());
    }
}
