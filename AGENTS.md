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
2. Create one workspace per task, based on `main@origin`:
   ```sh
   jj git fetch
   jj workspace add ~/workspaces/<slug> -r main@origin
   ```
   `<slug>` = short task name, e.g. `terrain-chunking`. Never base on
   `default`'s tip — it carries the human's unmerged work and produces
   unmergeable PRs. If a task needs code not yet on `main`, tell the human
   to land it first instead of basing on it.
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
- `CARGO_TARGET_DIR` is per-workspace (`<ws>/target`): cargo keys path-dep
  artifacts by crate name, not workspace directory — a shared target dir lets
  divergent sibling workspaces poison each other's build artifacts (observed:
  phantom stale-API compile errors). Cold builds are the price of isolation;
  never point two workspaces at the same target dir.
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

### Pull requests (optional review layer)

The human may ask for a GitHub PR instead of an in-workspace handoff. Then:

```sh
jj bookmark create <slug>                     # once, on your tip change
jj git push --bookmark <slug> --allow-new
gh pr create --head <slug> --base main --title "<type>: <what>" \
    --body-file pr-body.md                    # body follows the template
```

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

Gameplay-affecting diffs SHOULD have evidence. What works today, cheapest
first:

- **Headless asserts** — `cargo run -p server --bin smoke` already runs a
  scripted client in-process; add assertions and paste the output. Preferred
  for logic; not visual.
- **Screenshots** — run the client on the host display
  (`DISPLAY=:0`/`WAYLAND_DISPLAY=wayland-1` are set in this environment),
  capture with `grim` (wayland) or `import -window root`, and link the file.
- **Clips** — `ffmpeg -f x11grab` / `wl-screenrec` can record the running
  client window. Works, but flaky under load; budget a few retries.

Video cannot be embedded in a PR body via `gh` alone — playable embeds
require an upload through the github.com web UI, which agents lack. Link
files from the repo wiki (`git push` to `<repo>.wiki.git`), a release
asset, or an artifacts host instead; image links render inline.
