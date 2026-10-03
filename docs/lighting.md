# Lighting

The client starts at 09:00 and completes a day in 20 real minutes. Sunrise is
06:00 and sunset is 18:00. The tilted sun orbit reaches 60 degrees at noon,
producing angled shadows even at midday. Direct light warms near the horizon;
the sky and ambient fill fade into a blue moonlit night.

Use `--time-of-day 17` to start in late afternoon. Use `--day-length 60` for a
one-minute preview, or `--day-length 0 --time-of-day 0` to hold midnight.
Hours must be in [0, 24); day length is a nonnegative number of seconds.

Time is currently client-local, starts on launch, and advances while the pause
menu is open. It affects presentation only. A shared persistent world clock
would require server replication.

Sun and moon alternate as shadow-casting directional lights, fading out at the
horizon. Four shadow cascades cover 192 metres, with the first ending at 16
metres to retain detail near the player. Shadow distance is independent of
terrain render distance.

The sky is a procedural dome, not a flat color: a horizon-to-zenith gradient
that warms at dawn and dusk, a sun glow and disk, a moon disk, and a star field
that fades in as the sun sets. A camera-centred sphere scaled to the camera's
far distance renders `sky.wgsl` behind all terrain, so the dome never occludes
world geometry. The flat `ClearColor` remains only as the fallback for the
frame before the dome spawns.

Implementation: `apps/client/src/lighting.rs` owns the day cycle and the sky
palette; `apps/client/src/sky.rs` and `sky.wgsl` render the dome. Night
lighting is intentionally stylized for visibility: 350-lux blue moonlight and a
low ambient fill.
