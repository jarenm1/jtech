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
