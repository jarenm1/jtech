# Agent instructions

## Non-negotiables

Enforced by hook and human review:

1. **One task = one workspace = one agent.** Never write, build, or test in `default` (`/home/jaren/jtech`). Always create `~/workspaces/<slug>` from `main@origin` first.
2. **Wrap every heavy command in `scripts/agent-scope.sh`.** Builds, tests, clippy, and client/server runs MUST execute inside the scope to keep CPU, memory, and the GPU bounded. Bare commands violate policy even when they succeed.
3. **Never touch the GPU without `--gpu`.** Windowed client runs serialize behind `scripts/agent-scope.sh --gpu --`. Prefer headless software rendering (`--headless --render-backend software`), which uses no GPU.

## Workspace isolation

The `default` workspace (`/home/jaren/jtech`) is reserved for the human. All agent work happens in dedicated jj workspaces under `~/workspaces/`.

### Starting a task

1. Check `jj root`:
   - `/home/jaren/jtech` (`default`) → create a workspace (step 2) before any write, build, or test.
   - `~/workspaces/<slug>` → proceed in the existing workspace.
2. Create one workspace per task from `main@origin`:
   ```sh
   scripts/agent-workspace.sh <slug>
   ```
   This creates `~/workspaces/<slug>`, points `target/` to `~/workspaces/.targets/<slug>` (preventing multi-GB nix store copies), and hardlink-seeds dependency artifacts from the newest built sibling. Never branch from `default`'s tip (contains unmerged human work).
3. Keep all writes (files, temp outputs, `cwd`) strictly inside `~/workspaces/<slug>`.

### Concurrency rules

- Never touch files, run builds, or mutate jj state in a workspace you did not create.
- Long-running processes must bind ephemeral ports (`--bind 127.0.0.1:0`), never default ports.

## Resource governance

The workstation has 8 cores and one shared GPU. `scripts/agent-scope.sh` applies per-scope burst limits and manages the aggregate `agents.slice` (700% CPU, 20G RAM across all agents).

Canonical commands (wrapper outermost):
```sh
scripts/agent-scope.sh -- direnv exec ~/workspaces/<slug> cargo check -p <crate>
scripts/agent-scope.sh -- direnv exec ~/workspaces/<slug> cargo test -p <crate>
scripts/agent-scope.sh -- direnv exec ~/workspaces/<slug> cargo build --workspace
scripts/agent-scope.sh --gpu -- direnv exec ~/workspaces/<slug> cargo run -p voxel-client -- --frames 300 --screenshot /tmp/shot.png
```

- Light commands (`cargo fmt`, `cargo metadata`, `jj`, `rg`) do not need wrapping.
- Un-scoped heavy commands are automatically rewritten by `.omp/hooks/pre/agent-scope.ts`, but explicit wrapping is required by convention.

## Commits & version control

- Use `jj` exclusively — never `git add`, `git commit`, `git checkout`, or `git stash`.
- Commit early and often:
  - `jj describe -m "feat: <summary>"` names the current commit.
  - `jj commit -m "feat: <summary>"` finalizes it and opens a fresh empty working copy.
- Snapshotting is automatic on every jj command.
- Use lowercase conventional prefixes (`feat:`, `fix:`, `refactor:`, `chore:`).
- Amend with `jj squash` rather than creating fix-up commits.
- Never edit `Cargo.lock` by hand. On rebase conflicts, regenerate with cargo.

## Environment & verification

- Run cargo commands through `direnv exec` (or `nix develop ~/workspaces/<slug> -c <cmd>`) to load the flake shell and sccache wrapper.
- Stick to one build target selection (`--workspace` or `-p <crate>`); mixing them recompiles dependencies and thrashes artifact caches.
- Minimum gate before reporting done:
  ```sh
  scripts/agent-scope.sh -- direnv exec ~/workspaces/<slug> cargo check -p <touched crates>
  scripts/agent-scope.sh -- direnv exec ~/workspaces/<slug> cargo test -p <touched crates>
  ```

## Handoff & cleanup

Report: workspace path, change ID(s), automated checks run, and exact commands for human manual verification. Leave the workspace intact for human review (`jj workspace forget <slug>` is run by the human).

Before yielding, reclaim build disk:
```sh
scripts/agent-cleanup.sh --apply         # migrate in-tree targets and remove orphaned caches
scripts/agent-cleanup.sh --apply --gc    # also clean nix store roots (if flake files changed)
```

## Pull requests (when requested)

If the human requests a GitHub PR:
```sh
jj bookmark create <slug>
jj git push --bookmark <slug>
gh pr create --head <slug> --base main --title "<type>: <summary>" --body-file /tmp/pr-body.md
```
Write `/tmp/pr-body.md` outside the workspace so it is not snapshotted into the commit. Rebase on linear history with `jj git fetch && jj rebase -b <slug> -d main@origin && jj git push --bookmark <slug>`.

### Visual evidence

Attach evidence only for changes with a visible surface, matching the medium to the effect:

| Change | Evidence |
|---|---|
| Behavior over time (movement, animation, interaction, timing) | **motion**: headless clip (GIF), or recorded host clip |
| Still visual (rendering, UI layout, colors, single frame) | **one image**: headless screenshot, or host screenshot |
| No visible surface (data, math, protocol, refactor, logic) | **none**: headless assert or test output |

Evidence tooling (cheapest first):
- **Headless asserts** — `cargo run -p server --bin smoke` runs scripted client logic in-process. Preferred for logic.
- **Headless captures** — `voxel-client --headless --render-backend software --frames N --screenshot out.png` renders on CPU (lavapipe). Default for stills; wrap in `scripts/agent-scope.sh --`.
- **Headless clips** — add `--clip out.gif --clip-fps N` to software render for motion GIFs (playable directly in PRs).
- **Host screenshots/clips** — only when headless is insufficient. Windowed runs MUST use `scripts/agent-scope.sh --gpu --`. Capture via `grim` (Wayland), `import` (X11), or `wl-screenrec`.
