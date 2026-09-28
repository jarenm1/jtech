# Agent instructions

## Workspace isolation

The `default` workspace is `/home/jaren/jtech` — the human works there. All
agent work happens in a dedicated jj workspace under `~/workspaces/`.
**One task = one workspace = one agent.**

### Before making any change

1. Run `jj root`.
   - Result is `/home/jaren/jtech` → you are in `default`. Create a workspace
     (step 2) before any write, build, or test.
   - Result is a path under `~/workspaces/` → continue there.
2. Create one workspace per task:
   ```sh
   jj workspace add ~/workspaces/<slug>
   ```
   `<slug>` = short task name, e.g. `terrain-chunking`. The new working-copy
   commit is based on the tip of `default`'s described history; pass `-r` to
   base on something else.
3. Keep every write inside `~/workspaces/<slug>`: `cwd`, file paths, temp
   files, build outputs. Reads of `/home/jaren/jtech` are fine; writes are
   not. Nothing enforces this but convention — check `jj root` before edits.

### Concurrency rules

- Never edit files, run builds, or mutate jj state (`new`, `squash`,
  `abandon`, `describe`, bookmark ops) inside a workspace you did not create —
  that includes `default`. Concurrent agents must never share a workspace.
- Long-running processes bind ephemeral ports (`--bind 127.0.0.1:0`), never
  the default port — it collides with other agents' servers and the human's.

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

- `.envrc` is tracked (`use flake` + shared `CARGO_TARGET_DIR`). Agent shells
  are not direnv-hooked, so run cargo/build/test through direnv:
  ```sh
  direnv allow ~/workspaces/<slug>   # once per workspace
  direnv exec ~/workspaces/<slug> cargo check -p <crate>
  ```
  If the parent env is unknown or the allow isn't in place, use
  `nix develop ~/workspaces/<slug> -c <cmd>` instead — no `allow` needed.
- `CARGO_TARGET_DIR` is shared (`~/workspaces/.cargo-target`): the bevy dep
  graph compiles once across all workspaces. A cargo run may block on another
  agent's build lock — wait for it; don't override the variable.
- Minimum gate before reporting done: `cargo check -p <touched crates>` (whole
  workspace when crate boundaries are unclear) plus `cargo test -p` for
  touched crates with tests. The compile gate is the agent's; gameplay/
  behavioral sign-off is the human's — state in your report exactly what
  needs manual verification and the command to run it.

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
