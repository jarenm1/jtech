#!/usr/bin/env bash
# Create a jj workspace for one task, with its build artifacts outside the
# flake source tree and seeded from a sibling's build.
#
#   scripts/agent-workspace.sh <slug>
#
# Two things make a fresh workspace cheap:
#
#   * `target` is a symlink to ~/workspaces/.targets/<slug>. A jj workspace has
#     no `.git`, so nix treats the directory as a plain path and copies the
#     whole tree into the store every time the flake is re-evaluated; with a
#     real `target/` that copy is tens of GB, with the symlink it is 25 bytes.
#     Cargo is unaffected: it still writes to `<workspace>/target`, through the
#     link.
#   * the new target dir is a hardlink farm of the most recently built sibling,
#     so cargo finds the whole dependency build already fresh and compiles only
#     the workspace crates. Hardlinks cost no disk, and cargo replaces files
#     rather than writing through the links, so the sibling is unaffected.
#
# See AGENTS.md, "Workspace isolation".
set -euo pipefail

slug="${1:-}"
if [[ -z "$slug" ]]; then
    echo "usage: scripts/agent-workspace.sh <slug>" >&2
    exit 2
fi

ws="$HOME/workspaces/$slug"
targets="$HOME/workspaces/.targets/$slug"

if [[ -e "$ws" ]]; then
    echo "agent-workspace: $ws already exists" >&2
    exit 1
fi

jj git fetch
mkdir -p "$targets"
jj workspace add "$ws" -r main@origin
ln -s "$targets" "$ws/target"
direnv allow "$ws" >/dev/null 2>&1 || true

# Seed from the newest sibling that is not mid-build. A partially written
# artifact would be hardlinked as-is and cargo would trust its fingerprint.
seed=""
for candidate in "$HOME/workspaces/.targets"/*/; do
    candidate="${candidate%/}"
    [[ "$candidate" == "$targets" ]] && continue
    [[ -d "$candidate/debug/deps" ]] || continue
    if pgrep -af 'cargo|rustc' 2>/dev/null | grep -qF -- "$candidate"; then
        continue
    fi
    if [[ -z "$seed" || "$candidate/debug/deps" -nt "$seed/debug/deps" ]]; then
        seed="$candidate"
    fi
done

if [[ -n "$seed" ]]; then
    cp -al "$seed/." "$targets/"
    echo "agent-workspace: seeded target from ${seed##*/}"
fi

echo "agent-workspace: created $ws"
echo "agent-workspace: target -> $targets"
