# Agent instructions

## Non-negotiables

Enforced, not advisory: a pre-tool hook rewrites heavy commands to comply, and
the human reviews violations.

1. **One task = one workspace = one agent.** Never write, build, or test in
   `default` (`/home/jaren/jtech`). Create `~/workspaces/<slug>` from
   `main@origin` first — see "Workspace isolation".
2. **Wrap every heavy command in `scripts/agent-scope.sh`.** Builds, tests,
   clippy, and any client/server run MUST execute inside the scope so CPU,
   memory, and the single GPU stay bounded across parallel agents — see
   "Resource governance". Bare `cargo`/`voxel-client`/`server` is a violation
   even when it works.
3. **Never touch the GPU without `--gpu`.** Windowed client runs serialize
   behind `scripts/agent-scope.sh --gpu --`; prefer headless software
   rendering, which opens no Vulkan device at all.

## Workspace isolation

The `default` workspace is `/home/jaren/jtech` — the human works there. All
agent work happens in a dedicated jj workspace under `~/workspaces/`.
**One task = one workspace = one agent.**

### Before making any change

1. Run `jj root`.
   - Result is `/home/jaren/jtech` → you are in `default`. Create a workspace
     (step 2) before any write, build, or test.
   - Result is a path under `~/workspaces/` → continue there.
2. Create one workspace per task, based on `main@origin`, from the repo root:
   ```sh
   scripts/agent-workspace.sh <slug>
   ```
   `<slug>` = short task name, e.g. `terrain-chunking`. The script runs
   `jj git fetch`, adds the workspace from `main@origin`, points its `target`
   at `~/workspaces/.targets/<slug>`, and seeds that target from the most
   recently built sibling so the dependency build is reused instead of
   repeated — see "Build artifacts live outside the workspace" and "Seeding a
   new workspace from a sibling". Never base on `default`'s tip — it carries
   the human's unmerged work and produces unmergeable PRs. If a task needs code
   not yet on `main`, tell the human to land it first instead of basing on it.
3. Keep every write inside `~/workspaces/<slug>`: `cwd`, file paths, temp
   files, build outputs. Reads of `/home/jaren/jtech` are fine; writes are
   not. Nothing enforces this but convention — check `jj root` before edits.

### Concurrency rules

- Never edit files, run builds, or mutate jj state (`new`, `squash`,
  `abandon`, `describe`, bookmark ops) inside a workspace you did not create —
  that includes `default`. Concurrent agents must never share a workspace.
- Long-running processes bind ephemeral ports (`--bind 127.0.0.1:0`), never
  the default port — it collides with other agents' servers and the human's.

### Resource governance

Agents share one workstation (8 cores, one GPU). **Every** build, test,
clippy, or client/server run MUST go through `scripts/agent-scope.sh` — it is
the only thing keeping N parallel agents from saturating the machine. Running
`cargo`/`voxel-client`/`server` bare is a violation even when it "works".

Canonical forms (the wrapper goes outermost, so the whole tree — direnv, nix,
rustc, the game — is capped):

```sh
scripts/agent-scope.sh -- direnv exec ~/workspaces/<slug> cargo check -p <crate>
scripts/agent-scope.sh -- direnv exec ~/workspaces/<slug> cargo test -p <crate>
scripts/agent-scope.sh --cpu-quota 200% -- cargo build
scripts/agent-scope.sh --gpu -- direnv exec ~/workspaces/<slug> cargo run -p voxel-client -- --frames 300 --screenshot /tmp/shot.png
```

- Per-scope defaults: `CPUQuota=800%` (a burst ceiling — one agent alone may
  use the whole machine), `MemoryMax=8G`, swap disabled. Override with
  `--cpu-quota`/`--memory-max` or `AGENT_CPU_QUOTA`/`AGENT_MEMORY_MAX`.
- Every scope also joins one shared `agents.slice` with an aggregate cap
  (`AGENT_SLICE_CPU_QUOTA`, default `700%`; `AGENT_SLICE_MEMORY_MAX`, default
  `20G`), so the *total* across agents stays bounded no matter how many run.
  The **slice** quota, not the per-scope quota, is what bounds N concurrent
  agents; `700%` leaves one core for the human's editor and rust-analyzer.
  `--no-slice` opts out; `--slice NAME` picks another slice. The script
  re-applies the slice caps on every invocation, so tune them with the
  `AGENT_SLICE_*` variables, not a systemd drop-in.
- `CARGO_BUILD_JOBS` scales to how many agents are running: one agent alone
  gets all 8 cores, six agents get 2 each. Memory is the binding constraint,
  not CPU — six agents at `-j8` would spawn 48 rustc processes and blow the
  slice memory cap, while the slice CPU quota already throttles throughput
  under contention. Override with `AGENT_CARGO_JOBS` or `--jobs`.
- `--gpu` takes an exclusive `flock` so only one agent touches the GPU at a
  time. CPU-only work — including `--headless --render-backend software` —
  does not need it.
- Prefer the headless software renderer for visual checks (see "Visual
  evidence"); it uses no GPU at all.
- Light commands (`cargo fmt`, `cargo metadata`, `jj`, `rg`) do not need the
  wrapper.

The `.omp/hooks/pre/agent-scope.ts` pre-tool hook rewrites un-scoped heavy
bash commands into the wrapped form automatically, so a forgotten wrapper is
corrected rather than ignored. It is a backstop, not a licence to skip the
wrapper.

### Commits

- This repo uses jj (colocated with git). Use `jj` exclusively — never
  `git add`, `git commit`, `git checkout`, or `git stash`.
- Commit early and often; each logical step gets its own commit:
  - `jj describe -m "feat: <what>"` names the current working-copy commit.
  - `jj commit -m "feat: <what>"` finalizes it and opens a fresh empty
    working-copy commit on top.
- Snapshotting is automatic on every jj command — no staging step needed.
- Match history style: lowercase conventional prefix (`feat:`, `fix:`,
  `refactor:`, `chore:`).
- Amend the previous commit with `jj squash` (moves working-copy changes into
  its parent) rather than piling up fix-up commits.
- `Cargo.lock`: don't add or upgrade dependencies unless the task requires it.
  On a rebase conflict, regenerate with a cargo invocation and re-check —
  never hand-edit.

### Environment and verification

- `.envrc` is tracked (`use flake`); cargo's target dir defaults to
  `<workspace>/target`, which is per-workspace. Agent shells are not
  direnv-hooked, so run cargo/build/test through direnv, inside the scope:
  ```sh
  direnv allow ~/workspaces/<slug>   # once per workspace
  scripts/agent-scope.sh -- direnv exec ~/workspaces/<slug> cargo check -p <crate>
  ```
  If the parent env is unknown or the allow isn't in place, use
  `nix develop ~/workspaces/<slug> -c <cmd>` instead — no `allow` needed —
  still wrapped: `scripts/agent-scope.sh -- nix develop ~/workspaces/<slug> -c cargo check -p <crate>`.
- `CARGO_TARGET_DIR` is per-workspace (`<ws>/target`): cargo keys path-dep
  artifacts by crate name, not workspace directory — a shared target dir lets
  divergent sibling workspaces poison each other's build artifacts (observed:
  phantom stale-API compile errors). Cold builds are the price of isolation;
  never point two workspaces at the same target dir.
- Minimum gate before reporting done, both wrapped in `scripts/agent-scope.sh`:
  `cargo check -p <touched crates>` (whole workspace when crate boundaries are
  unclear) plus `cargo test -p` for touched crates with tests. The compile gate
  is the agent's; gameplay/behavioral sign-off is the human's — state in your
  report exactly what needs manual verification and the command to run it.
- Pick one build selection and keep it. `cargo build --workspace` and
  `cargo build -p <crate>` resolve features differently, so switching between
  them recompiles shared deps and leaves *both* artifact sets in `target/debug`
  (observed: 138 crates recompiled, two `libbevy_app-*.rlib` side by side).
  `--workspace` is the stable superset; `-p <crate>` is faster per invocation
  but thrashes against a `--workspace` build.

#### Build artifacts live outside the workspace

In a jj workspace, `<ws>/target` is a **symlink** to
`~/workspaces/.targets/<slug>`. Cargo is unaffected — it still writes to
`<ws>/target`, through the link — but nix is:

- A jj workspace has no `.git`, so nix treats the directory as a plain path and
  copies the *entire* tree into the store every time the flake is re-evaluated
  (a `flake.nix`/`flake.lock` change, or a fresh `direnv allow`). With a real
  `target/` in the tree that copy is 10-35G per workspace; with the symlink it
  is 25 bytes. nix does not follow the link, and `.gitignore` is not consulted
  for non-git paths.
- The copy is GC-rooted by `<ws>/.direnv/flake-inputs/`, so it does not go away
  on its own. `scripts/agent-cleanup.sh --apply --gc` drops those roots and
  runs `nix store gc`.
- If the symlink is missing (a workspace created by hand), cargo writes a real
  `target/` back into the tree. `scripts/agent-cleanup.sh --apply` migrates it
  back out — a rename, so it is instant and loses nothing.
- The external directory must exist; a dangling symlink makes cargo fail. The
  workspace script creates it, and the cleanup script recreates it on demand.

#### Seeding a new workspace from a sibling

`scripts/agent-workspace.sh` fills the new target dir with **hardlinks** to the
most recently built sibling's target. Cargo then finds the whole dependency
build already fresh and compiles only the workspace crates:

| fresh workspace, `cargo build --workspace` | wall |
|---|---|
| cold, nothing to reuse | 9m33s |
| sccache only | 8m58s |
| seeded from a sibling | **46s** |

- Hardlinks cost no disk: the sibling's artifacts are shared, not copied, and
  `du` double-counts them. Cargo replaces files rather than writing through the
  links, so the sibling is never corrupted (verified: the sibling's binary keeps
  its own inode and behaviour).
- The seed is skipped when every sibling is mid-build, because a partially
  written artifact would be hardlinked as-is and cargo would trust its
  fingerprint.
- A seed from a workspace on a different `Cargo.lock` or profile is simply
  partially stale: cargo rebuilds what does not match.
- The first workspace on a machine has no sibling to seed from and pays the
  cold build once.

### Compile caching

Seeding (above) is the primary mechanism for a cheap fresh workspace. sccache
is the fallback for when there is no sibling to seed from, or the seed is
stale: `.cargo/config.toml` sets `rustc-wrapper = "sccache"`, so registry
crates are served from one shared content-addressed cache.

- The cache lives in `$SCCACHE_DIR` (`~/.cache/sccache`), capped by
  `SCCACHE_CACHE_SIZE` (10G); both are set by the flake dev shell. sccache
  evicts least-recently-used entries past the cap, so it cannot grow without
  bound.
- It is worth less than it looks: sccache refuses to cache `bin` and
  `proc-macro` crates, and those are a large share of a Bevy build. Measured on
  a fresh workspace: 9m33s cold, 8m58s with sccache, 46s seeded. Do not rely on
  it for the fresh-workspace case.
- Workspace members keep incremental compilation (sccache passes those
  through), so warm rebuilds are unaffected either way.
- `sccache --show-stats` reports hit rate and cache size.
- `sccache` is provided by the flake dev shell. Because the wrapper is set in
  `.cargo/config.toml`, a `cargo` invocation outside the dev shell fails
  loudly with "sccache: not found" — run it through `direnv exec` (or
  `nix develop`) as above.
- The server's `SCCACHE_DIR` is fixed when the server starts, and a client with
  a different value silently uses the running server's cache. After changing
  it, run `sccache --stop-server`.

### Handoff to the human

Report: workspace path, change id(s), checks run, and what to verify
manually. The human reviews inside `~/workspaces/<slug>` directly, or checks
out a detached copy for a clean run:

```sh
jj workspace add ~/workspaces/review-<slug> -r <change-id>
```

After the human returns findings, resume in the same workspace and
`jj squash` fixes into the relevant commit. Leave the workspace in place when
reporting done — the human runs `jj workspace forget <slug>` after review.

#### Reclaiming disk

Every workspace keeps its own target dir, and a full one runs 10-35G, so
abandoned workspaces and stale nix copies are the main disk consumers. Before
reporting done, run:

```sh
scripts/agent-cleanup.sh                 # report what is reclaimable
scripts/agent-cleanup.sh --apply         # migrate + delete
scripts/agent-cleanup.sh --apply --gc    # also drop stale nix roots and GC
```

- **migrate** — moves a real in-tree `target/` out to
  `~/workspaces/.targets/<slug>` and symlinks it back (a rename, so instant).
- **orphaned** — deletes target dirs with no registered jj workspace behind
  them, plus the abandoned shared `~/workspaces/.cargo-target`.
- **stale** — deletes `incremental` and `examples` for workspaces idle longer
  than `--stale-days` (default 7).
- **sweep** (`--sweep`) — runs `cargo-sweep` over `deps` for those same
  workspaces, dropping artifacts older than `--stale-days`. Where superseded
  dependency versions pile up; costs a recompile (from the sccache cache) on
  the next build, so it is opt-in and needs the dev shell on PATH.
- **gcroot** (`--gc`) — drops nix-direnv roots for copies of this repo, then
  runs `nix store gc`. Each flake change otherwise leaves a GC-rooted copy of
  every workspace in the store.

It never touches `deps` or the built binaries of a live workspace, so the next
warm build stays fast. The `.omp/hooks/post/` hook runs the same sweep
(`--apply --quiet`, no GC) at session shutdown.

### Pull requests (optional review layer)

The human may ask for a GitHub PR instead of an in-workspace handoff. Then:

```sh
jj bookmark create <slug>                     # once, on your tip change
jj git push --bookmark <slug>
gh pr create --head <slug> --base main --title "<type>: <what>" \
    --body-file /tmp/pr-body.md               # body follows the template
```

Keep the body file outside the repo (`/tmp/` or `~`) — jj snapshots every
command, so a `pr-body.md` written inside the workspace lands in the commit
and gets pushed.

Rebase flow (linear history — repo enforces rebase-only merges, no merge or
squash commits):

```sh
jj git fetch
jj rebase -b <slug> -d main@origin   # keep the branch on fresh main
jj git push --bookmark <slug>
```

- Follow `.github/pull_request_template.md` exactly. Automated checks must
  list the real commands run and their results — never claim checks you
  didn't run. Manual-verification steps must be concrete enough to execute
  blind (binary, flags, expected behavior).
- After `jj squash` fixes, `jj git push --bookmark <slug>` again; jj
  force-pushes rewritten commits automatically. Never `--force` yourself,
  never merge or close the PR — the human owns the merge.
- `gh` runs under the human's account; the PR is public-facing. Don't push
  scratch or half-failed work — push once the local gate above passes.

#### Visual evidence

Attach evidence only for changes with a visible surface, and match the medium
to what the change affects:

| Change | Evidence |
|---|---|
| Behavior over time — movement, animation, interaction, timing | **motion**: a headless clip (GIF), or a recorded clip when headless cannot show it |
| Still visual — rendering, UI layout, colors, a single frame | **one image**: a headless screenshot, or a host screenshot when headless cannot show it |
| No visible surface — data, math, protocol, refactor, logic | **none**: a headless assert or test output is the evidence |

A still cannot show motion and a clip wastes review time on a static change;
pick the one that matches. Never attach an image to prove something a test
already proves.

What works today, cheapest first:

- **Headless asserts** — `cargo run -p server --bin smoke` already runs a
  scripted client in-process; add assertions and paste the output. Preferred
  for logic; not visual.
- **Headless captures** — `voxel-client --headless --render-backend software
  --frames N --screenshot out.png` renders offscreen on the CPU (lavapipe), so
  it needs no display server and no GPU. This is the default for still visual
  evidence; wrap it in `scripts/agent-scope.sh --` (CPU cap; no `--gpu`
  needed). Slower than the real GPU: fine for stills, not for frame-time
  numbers.
- **Headless clips** — add `--clip out.gif --clip-fps N` for motion — the
  default for gameplay/behavioral evidence. The GIF is encoded in-process (no
  ffmpeg needed) and plays inline in a PR body, unlike a video file. Sampling
  is frame-counted, so the clip plays at `--clip-fps` regardless of how fast
  the renderer runs.
- **Screenshots** — only when headless cannot show what you need. Run the
  client on the host display (`DISPLAY=:0`/`WAYLAND_DISPLAY=wayland-1` are set
  in this environment) and capture with `grim` (wayland) or
  `import -window root`. Windowed runs MUST be wrapped in
  `scripts/agent-scope.sh --gpu --` so they serialize on the one GPU.
- **Clips** — `ffmpeg -f x11grab` / `wl-screenrec` can record the running
  client window. Works, but flaky under load; budget a few retries.

Video cannot be embedded in a PR body via `gh` alone — playable embeds
require an upload through the github.com web UI, which agents lack. Link
files from the repo wiki (`git push` to `<repo>.wiki.git`), a release
asset, or an artifacts host instead; image links render inline.
