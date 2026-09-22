# Agent instructions

## Workspace isolation

The `default` workspace is `/home/jaren/jtech` — the human works there. All agent work happens in a dedicated jj workspace under `~/workspaces/`.

### Before making any change

1. Run `jj root`.
   - Result is `/home/jaren/jtech` → you are in `default`. Create a workspace (step 2) before any write, build, or test.
   - Result is a path under `~/workspaces/` → continue there.
2. Create one workspace per task:
   ```sh
   jj workspace add ~/workspaces/<slug>
   printf 'use flake\nexport CARGO_TARGET_DIR=$HOME/workspaces/.cargo-target\n' > ~/workspaces/<slug>/.envrc && direnv allow ~/workspaces/<slug>
   ```
   `<slug>` = short task name, e.g. `terrain-chunking`. The new working-copy commit sits on top of `default`'s parent revision, so fresh workspaces always start from the current state. `.envrc` is untracked — each workspace needs its own copy.
3. Agent shells are not direnv-hooked, so `cd` does not load `.envrc`. Run cargo/build/test commands through direnv:
   ```sh
   direnv exec ~/workspaces/<slug> cargo build
   ```
   `direnv exec` loads the workspace's `.envrc` for the command and works in non-interactive shells. `CARGO_TARGET_DIR` shares one build cache across all agent workspaces (deps compile once; concurrent builds serialize on cargo's lock) while keeping `default`'s `target/` untouched.
4. Keep every write inside `~/workspaces/`: `cwd`, file paths, `CARGO_TARGET_DIR`, temp files. Reads of `/home/jaren/jtech` are fine; writes are not.

### Commits

- This repo uses jj (colocated with git). Use `jj` exclusively — never `git add`, `git commit`, `git checkout`, or `git stash`.
- Commit early and often; each logical step gets its own commit:
  - `jj describe -m "feat: <what>"` names the current working-copy commit.
  - `jj commit -m "feat: <what>"` finalizes it and opens a fresh empty working-copy commit on top.
- Snapshotting is automatic on every jj command — no staging step needed.
- Match history style: lowercase conventional prefix (`feat:`, `fix:`, `refactor:`, `chore:`).
- Amend the previous commit with `jj squash` (moves working-copy changes into its parent) rather than piling up fix-up commits.

### Finishing

- Workspaces share the repo, so your commits appear in `jj log` from `default` automatically. Leave your described commit(s) as the result — an empty working-copy commit on top is normal.
- Report the workspace path and change id so the human can `jj workspace forget <name>` after review.
