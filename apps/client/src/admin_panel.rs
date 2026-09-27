//! Local admin console: backquote opens a flag list plus a `name=value`
//! command line. Everything routes through `AdminFlags::apply`, the same
//! language the server's stdin commands and `--set` flags use.
use std::collections::{HashMap, VecDeque};

use admin::{AdminFlags, FlagValue};
use bevy::{input::keyboard::KeyboardInput, prelude::*};

use crate::{ClientSession, lighting::DayCycle, pause_menu};

const MAX_LOG_LINES: usize = 6;

#[derive(Resource, Default)]
pub(crate) struct AdminPanel {
    pub open: bool,
    /// Edited command line, sent to `AdminFlags::apply` on Enter.
    input: String,
    /// Recent command results, oldest first.
    log: VecDeque<String>,
    /// Esc ate by this overlay must not reach `pause_menu::input` in the same
    /// frame, so closing is remembered for the rest of the tick.
    pub closed_this_frame: bool,
    /// Last revision applied per flag; resources are pushed only on change so
    /// live systems (`DayCycle` keeps ticking) are not re-snapshotted.
    applied: HashMap<String, u64>,
}

#[derive(Component)]
pub(crate) struct AdminPanelNode;

#[derive(Component)]
pub(crate) struct AdminPanelText;

pub(crate) fn spawn(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(12),
                top: px(12),
                width: px(460),
                max_width: percent(60),
                padding: UiRect::all(px(14)),
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(0.03, 0.04, 0.05, 0.82)),
            GlobalZIndex(120),
            bevy::ui::FocusPolicy::Block,
            AdminPanelNode,
        ))
        .with_child((
            Text::default(),
            TextFont {
                font_size: 13.0,
                ..default()
            },
            TextColor(Color::srgb(0.88, 0.9, 0.82)),
            AdminPanelText,
        ));
}

/// Backquote toggles the console, Enter runs the buffered command, Esc closes.
/// `pause_menu::input` consults `closed_this_frame` so one Esc never peels two
/// layers.
pub(crate) fn input(
    keys: Res<ButtonInput<KeyCode>>,
    mut events: MessageReader<KeyboardInput>,
    window: Single<&Window>,
    menu: Res<pause_menu::PauseMenu>,
    mut panel: ResMut<AdminPanel>,
    mut flags: ResMut<AdminFlags>,
) {
    panel.closed_this_frame = false;
    if !window.focused {
        panel.open = false;
        return;
    }
    if !panel.open {
        // Buffered keystrokes from while the console was closed are stale.
        events.clear();
        if keys.just_pressed(KeyCode::Backquote) && !menu.open {
            panel.open = true;
            panel.input.clear();
        }
        return;
    }
    for event in events.read() {
        if !event.state.is_pressed() {
            continue;
        }
        match event.key_code {
            KeyCode::Enter | KeyCode::NumpadEnter => {
                let command = std::mem::take(&mut panel.input);
                let entry = match flags.apply(&command) {
                    Ok(reply) => format!("> {command}\n{reply}"),
                    Err(error) => format!("> {command}\n! {error}"),
                };
                panel.push_log(entry);
            }
            KeyCode::Backspace => {
                panel.input.pop();
            }
            _ => {
                if let Some(text) = &event.text {
                    panel.input.push_str(text);
                }
            }
        }
    }
    if keys.just_pressed(KeyCode::Backquote) || keys.just_pressed(KeyCode::Escape) {
        panel.open = false;
        panel.closed_this_frame = keys.just_pressed(KeyCode::Escape);
    }
}

impl AdminPanel {
    fn push_log(&mut self, entry: String) {
        self.log.push_back(entry);
        while self.log.len() > MAX_LOG_LINES {
            self.log.pop_front();
        }
    }
}

/// Rebuild the overlay text and push changed flag values into the resources
/// they drive. Runs every frame; the revision map keeps writes one-shot.
pub(crate) fn sync(
    flags: Res<AdminFlags>,
    mut panel: ResMut<AdminPanel>,
    mut cycle: ResMut<DayCycle>,
    mut session: ResMut<ClientSession>,
    mut node: Single<&mut Node, With<AdminPanelNode>>,
    mut text: Single<&mut Text, With<AdminPanelText>>,
) {
    node.display = if panel.open {
        Display::Flex
    } else {
        Display::None
    };
    for view in flags.list() {
        if panel
            .applied
            .get(view.name)
            .is_some_and(|&revision| revision >= view.revision)
        {
            continue;
        }
        let applied = match (view.name, view.value) {
            ("day_length", FlagValue::Float(seconds)) => {
                cycle.day_seconds = *seconds;
                true
            }
            ("time_of_day", FlagValue::Float(hour)) => {
                cycle.hour = hour.rem_euclid(24.0);
                true
            }
            ("noclip", FlagValue::Bool(enabled)) => {
                session.noclip_requested = *enabled;
                true
            }
            _ => false,
        };
        if applied {
            panel.applied.insert(view.name.to_string(), view.revision);
        }
    }
    if panel.open {
        let mut lines = String::from(
            "FLAGS — `set <name> <value>`, `toggle <name>`, `get <name>`; ` or Esc closes\n",
        );
        for view in flags.list() {
            let read_only = if view.mutable { "" } else { "  (startup)" };
            lines.push_str(&format!("{} = {}{}\n", view.name, view.value, read_only));
        }
        for entry in &panel.log {
            lines.push_str(entry);
            lines.push('\n');
        }
        lines.push_str(&format!("> {}█", panel.input));
        text.0 = lines;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.init_resource::<AdminPanel>()
            .init_resource::<AdminFlags>()
            .init_resource::<ClientSession>()
            .insert_resource(DayCycle::default())
            .add_systems(Startup, |mut commands: Commands| spawn(&mut commands))
            .add_systems(Update, sync);
        {
            let mut flags = app.world_mut().resource_mut::<AdminFlags>();
            flags.register("day_length", FlagValue::Float(1200.0), "");
            flags.register("time_of_day", FlagValue::Float(9.0), "");
            flags.register("noclip", FlagValue::Bool(false), "");
        }
        app.update();
        app
    }

    #[test]
    fn changed_flags_push_into_resources_once() {
        let mut app = app();
        let mut flags = app.world_mut().resource_mut::<AdminFlags>();
        flags.apply("set day_length 5").unwrap();
        flags.apply("toggle noclip").unwrap();
        app.update();
        assert_eq!(app.world().resource::<DayCycle>().day_seconds, 5.0);
        assert!(app.world().resource::<ClientSession>().noclip_requested);
        // A system advancing the clock between writes is not clobbered: the
        // flag revision is unchanged, so the push stays one-shot.
        app.world_mut().resource_mut::<DayCycle>().hour = 22.0;
        app.update();
        assert_eq!(app.world().resource::<DayCycle>().hour, 22.0);
        app.world_mut()
            .resource_mut::<AdminFlags>()
            .apply("set time_of_day 3")
            .unwrap();
        app.update();
        assert_eq!(app.world().resource::<DayCycle>().hour, 3.0);
    }

    #[test]
    fn panel_renders_flags_and_hides_when_closed() {
        let mut app = app();
        assert!(!app.world().resource::<AdminPanel>().open);
        app.world_mut().resource_mut::<AdminPanel>().open = true;
        app.world_mut()
            .resource_mut::<AdminFlags>()
            .apply("set day_length bogus")
            .unwrap_err();
        app.update();
        let display = app
            .world_mut()
            .query_filtered::<&Node, With<AdminPanelNode>>()
            .single(app.world())
            .unwrap()
            .display;
        assert_eq!(display, Display::Flex);
        let text = app
            .world_mut()
            .query_filtered::<&Text, With<AdminPanelText>>()
            .single(app.world())
            .unwrap();
        assert!(text.0.contains("day_length = 1200"));
        assert!(text.0.contains("noclip = false"));
    }
}
