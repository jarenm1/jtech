//! Package audio: plays the sounds a loaded package authors on the matching
//! event. Packages ship OGG/WAV files under `assets/` and declare them in
//! `server.scm`; the client resolves them through the same `pkg://` asset
//! source the models use, so there is nothing to wire per sound.
use bevy::audio::{AudioPlayer, AudioSource, PlaybackSettings};
use bevy::prelude::*;
use protocol::SoundInfo;

use crate::{ClientSession, package_assets::PackageAssets};

/// Sounds loaded packages author, keyed by event.
#[derive(Resource, Default)]
pub(crate) struct PackageSounds {
    entries: Vec<SoundInfo>,
}

impl PackageSounds {
    pub(crate) fn receive(&mut self, sounds: &[SoundInfo]) {
        self.entries = sounds.to_vec();
    }

    /// The `pkg://` URI a loaded package authors for an event, if any.
    fn uri(&self, assets: &PackageAssets, event: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|sound| sound.event == event)
            .and_then(|sound| assets.uri(&sound.package, &sound.path))
            .map(|uri| uri.to_string())
    }
}

/// A request to play a package sound for an event. Emitters only name the
/// event; the sound table and the asset cache decide whether anything plays.
#[derive(Message)]
pub(crate) struct PlaySound {
    pub event: &'static str,
    pub position: Option<Vec3>,
}

/// Play every requested package sound. A missing sound is a no-op, so a package
/// that ships no audio stays silent rather than erroring.
pub(crate) fn play(
    mut commands: Commands,
    mut requests: MessageReader<PlaySound>,
    asset_server: Res<AssetServer>,
    sounds: Res<PackageSounds>,
    assets: Res<PackageAssets>,
) {
    for request in requests.read() {
        let Some(uri) = sounds.uri(&assets, request.event) else {
            continue;
        };
        let source: Handle<AudioSource> = asset_server.load(uri);
        let mut entity = commands.spawn((AudioPlayer::new(source), PlaybackSettings::DESPAWN));
        if let Some(position) = request.position {
            entity.insert(Transform::from_translation(position));
        }
    }
}

/// A looping package sound, keyed by the state event that keeps it alive.
#[derive(Component)]
pub(crate) struct SoundLoop(&'static str);

/// The loop events active this frame, from the predicted state. These are
/// states the client owns, so a loop starts and stops on the same frame the
/// state changes — no round trip.
fn active_loops(session: &ClientSession) -> impl Iterator<Item = &'static str> {
    [
        (session.state.drawing, "drawing"),
        (session.health.is_depleted(), "dead"),
    ]
    .into_iter()
    .filter_map(|(active, event)| active.then_some(event))
}

/// Keep exactly the active looping sounds playing: start the ones that became
/// active, stop the ones that ended. A package that authors no loop for an
/// event stays silent.
pub(crate) fn loops(
    mut commands: Commands,
    session: Res<ClientSession>,
    asset_server: Res<AssetServer>,
    sounds: Res<PackageSounds>,
    assets: Res<PackageAssets>,
    playing: Query<(Entity, &SoundLoop)>,
) {
    let active: Vec<&'static str> = active_loops(&session).collect();
    for (entity, sound) in &playing {
        if !active.contains(&sound.0) {
            commands.entity(entity).despawn();
        }
    }
    for event in active {
        if playing.iter().any(|(_, sound)| sound.0 == event) {
            continue;
        }
        let Some(uri) = sounds.uri(&assets, event) else {
            continue;
        };
        let source: Handle<AudioSource> = asset_server.load(uri);
        commands.spawn((
            AudioPlayer::new(source),
            PlaybackSettings::LOOP,
            SoundLoop(event),
        ));
    }
}

/// Keep the sound table in step with the replicated package manifest.
pub(crate) fn sync(
    session: Res<ClientSession>,
    mut sounds: ResMut<PackageSounds>,
    mut shown: Local<Option<u64>>,
) {
    let Some(revision) = session.packages.revision else {
        return;
    };
    if *shown == Some(revision) {
        return;
    }
    *shown = Some(revision);
    sounds.receive(&session.packages.sounds);
}
