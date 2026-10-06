#!/usr/bin/env bash
# Reclaim disk from agent build artifacts.
#
#   scripts/agent-cleanup.sh                 # report only, changes nothing
#   scripts/agent-cleanup.sh --apply         # migrate + delete
#   scripts/agent-cleanup.sh --apply --gc    # also drop stale nix roots and GC
#   scripts/agent-cleanup.sh --apply --quiet # skip the size walk (used by the
#                                            # session-shutdown hook)
#   scripts/agent-cleanup.sh --apply --all   # also drop incremental for active workspaces
#   scripts/agent-cleanup.sh --apply --sweep # also sweep old `deps` artifacts
#   scripts/agent-cleanup.sh --apply --stale-days 3
#
# Categories:
#
#   migrate   a registered workspace whose `target` is still a real directory:
#             move it to ~/workspaces/.targets/<slug> and replace it with a
#             symlink. A jj workspace has no `.git`, so nix copies the whole
#             tree into the store on every flake re-evaluation; with the
#             symlink that copy is 25 bytes instead of tens of GB. The move is
#             a rename, so it is instant and loses nothing.
#   orphaned  ~/workspaces/.targets/<slug>, or an in-tree target, with no
#             registered jj workspace behind it.
#   link      a registered workspace with no `target` at all: create the symlink
#             so its first build lands outside the flake source tree.
#   legacy    ~/workspaces/.cargo-target, the abandoned shared target dir.
#             Shared target dirs poison sibling workspaces with stale
#             artifacts; see AGENTS.md, "Environment and verification".
#   stale     `incremental` and `examples` of a workspace idle longer than
#             --stale-days (default 7). Both are regenerated on the next build
#             and neither is needed to run or review a binary.
#   sweep     (`--sweep`) `deps` artifacts older than --stale-days, via
#             cargo-sweep. Where superseded dependency versions pile up; costs
#             a recompile (from the sccache cache) on the next build, so it is
#             opt-in and needs the flake dev shell on PATH.
#   gcroot    (`--gc`) nix-direnv roots pointing at a copy of this repository.
#             Dropping them lets `nix store gc` reclaim the copies; the next
#             direnv evaluation re-creates a root for a now-tiny tree.
#
# The built binaries and `deps` of a live workspace are never touched by
# default: they are what makes the next warm build fast. `--sweep` is the
# opt-in exception.
#
# Environment: WORKSPACES_DIR (default ~/workspaces), REPO_ROOT (default the
# jj root of the current directory).
set -euo pipefail

apply=0
all=0
quiet=0
gc=0
sweep=0
stale_days=7
workspaces_dir="${WORKSPACES_DIR:-$HOME/workspaces}"

usage() {
    sed -n '2,44p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --apply) apply=1; shift ;;
        --all) all=1; shift ;;
        --quiet) quiet=1; shift ;;
        --gc) gc=1; shift ;;
        --sweep) sweep=1; shift ;;
        --stale-days) stale_days="$2"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) echo "agent-cleanup: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done

repo_root="${REPO_ROOT:-$(jj root 2>/dev/null || true)}"
if [[ -z "$repo_root" ]]; then
    echo "agent-cleanup: not inside a jj repo (set REPO_ROOT)" >&2
    exit 1
fi

# Registered workspace paths and their slugs, one per line.
registered_paths=""
registered_slugs=""
if command -v jj >/dev/null 2>&1; then
    registered_paths=$(cd "$repo_root" && jj workspace list 2>/dev/null |
        sed -n 's/^[^:]*: \([^ ]*\) .*/\1/p' |
        while read -r p; do (cd "$repo_root" && cd "$p" 2>/dev/null && pwd); done || true)
    registered_slugs=$(sed 's|.*/||' <<<"$registered_paths")
fi

is_registered_path() {
    [[ -n "$registered_paths" ]] && grep -qxF "$1" <<<"$registered_paths"
}

is_registered_slug() {
    [[ -n "$registered_slugs" ]] && grep -qxF "$1" <<<"$registered_slugs"
}

human() { numfmt --to=iec --suffix=B "$1" 2>/dev/null || echo "${1}B"; }
dir_bytes() { du -sb "$1" 2>/dev/null | cut -f1 || echo 0; }

# A workspace with a compiler or linker running against it must not have its
# target moved out from under the build. Directory mtimes are unreliable here
# (artifacts are written into `deps/`, not `target/debug/`), and the link step
# runs `cc`, not `rustc`, so ask the process table for all of them.
workspace_busy() {
    local ws="$1" pid
    for pid in $(pgrep -x 'cargo|rustc|cc|gcc|clang|ld|mold|collect2' 2>/dev/null || true); do
        [[ -r "/proc/$pid/cmdline" ]] || continue
        if tr '\0' ' ' < "/proc/$pid/cmdline" | grep -qF -- "$ws"; then
            return 0
        fi
    done
    return 1
}

reclaimed=0
report=()
sizes=()

note() {
    local path="$1" reason="$2" bytes="${3:-}"
    if ((quiet)); then
        report+=("$(printf '%-10s %s' "$reason" "$path")")
    elif [[ -n "$bytes" ]]; then
        report+=("$(printf '%-10s %-9s %s' "$reason" "$(human "$bytes")" "$path")")
    else
        report+=("$(printf '%-10s %-9s %s' "$reason" "$(human "$(dir_bytes "$path")")" "$path")")
    fi
}

drop() {
    local path="$1" reason="$2"
    [[ -e "$path" || -L "$path" ]] || return 0
    local bytes=0
    if ((quiet)); then
        report+=("$(printf '%-10s %s' "$reason" "$path")")
    else
        bytes=$(dir_bytes "$path")
        report+=("$(printf '%-10s %-9s %s' "$reason" "$(human "$bytes")" "$path")")
    fi
    if ((apply)); then
        rm -rf -- "$path"
    fi
    reclaimed=$((reclaimed + bytes))
}

# The abandoned shared target dir is not a workspace and the glob below skips
# it (leading dot). It *is* a target dir root, so it has no `target/` child.
drop "$workspaces_dir/.cargo-target" "legacy"

now=$(date +%s)
stale_secs=$((stale_days * 86400))

for dir in "$workspaces_dir"/*/; do
    [[ -d "$dir" ]] || continue
    dir="${dir%/}"
    slug="${dir##*/}"
    target="$dir/target"

    if ! is_registered_path "$dir"; then
        [[ -e "$target" ]] && drop "$target" "orphaned"
        continue
    fi

    # Migrate an in-tree target to the external store and symlink it back.
    if [[ -d "$target" && ! -L "$target" ]]; then
        if workspace_busy "$dir"; then
            report+=("$(printf '%-10s %s' "busy" "$target")")
        else
            external="$workspaces_dir/.targets/$slug"
            note "$target" "migrate"
            if ((apply)); then
                mkdir -p "$(dirname "$external")"
                if [[ -e "$external" ]]; then
                    # Both exist, so the symlink was missing and cargo has been
                    # writing to the in-tree copy: that one is current.
                    rm -rf -- "$external"
                fi
                mv -- "$target" "$external"
                ln -s "$external" "$target"
            fi
        fi
    fi

    # A workspace that never built has no `target` at all. Create the symlink so
    # its first build lands outside the flake source tree instead of growing a
    # real directory that nix would then copy into the store.
    if [[ ! -e "$target" && ! -L "$target" ]]; then
        report+=("$(printf '%-10s %s' "link" "$target")")
        if ((apply)); then
            mkdir -p "$workspaces_dir/.targets/$slug"
            ln -s "$workspaces_dir/.targets/$slug" "$target"
        fi
    fi

    # Resolve the effective target dir (through the symlink, if any).
    effective="$target"
    if [[ -L "$target" ]]; then
        effective=$(readlink -f "$target")
        # Cargo cannot create the directory through a dangling symlink, so a
        # missing external dir breaks every build in the workspace.
        if [[ ! -d "$effective" ]]; then
            note "$target" "repair"
            ((apply)) && mkdir -p "$effective"
        fi
    fi
    [[ -d "$effective/debug" ]] || continue

    if ((quiet)); then
        sizes+=("$dir")
    else
        sizes+=("$(printf '%-9s %s' "$(human "$(dir_bytes "$effective")")" "$dir")")
    fi

    newest=$(find "$effective/debug" -maxdepth 1 -printf '%T@\n' 2>/dev/null | sort -n | tail -1 | cut -d. -f1)
    newest="${newest:-0}"
    if ((all)) || ((now - newest > stale_secs)); then
        drop "$effective/debug/incremental" "stale"
        drop "$effective/debug/examples" "stale"
        # Deeper pass over `deps`: removes artifacts older than --stale-days,
        # which is where superseded dependency versions pile up. Opt-in because
        # it costs a recompile (from the sccache cache) on the next build.
        # cargo-sweep takes the project dir, not the target dir.
        if ((sweep)) && command -v cargo-sweep >/dev/null 2>&1; then
            note "$effective" "sweep"
            if ((apply)); then
                cargo-sweep sweep --time "$stale_days" "$dir" >/dev/null 2>&1 || true
            fi
        fi
    fi
done

# External target dirs with no workspace behind them.
for dir in "$workspaces_dir"/.targets/*/; do
    [[ -d "$dir" ]] || continue
    dir="${dir%/}"
    is_registered_slug "${dir##*/}" || drop "$dir" "orphaned"
done

if ((gc)); then
    for root in "$workspaces_dir"/*/.direnv/flake-inputs/*-source; do
        [[ -L "$root" ]] || continue
        store_path=$(readlink "$root")
        # A copy of this repo, not a flake input like nixpkgs — which also
        # ships an AGENTS.md and Cargo.toml at its root.
        if [[ -d "$store_path/crates/voxel_world" && -d "$store_path/apps/client" ]]; then
            note "$root" "gcroot" "$(dir_bytes "$store_path")"
            reclaimed=$((reclaimed + $(dir_bytes "$store_path")))
            ((apply)) && rm -f -- "$root"
        fi
    done
    if ((apply)); then
        nix store gc >/dev/null 2>&1 || true
    fi
fi

if ((${#report[@]} > 0)); then
    printf '%s\n' "${report[@]}"
else
    echo "agent-cleanup: nothing to reclaim"
fi

if ((apply)); then
    if ((quiet)); then
        echo "agent-cleanup: reclaimed ${#report[@]} path(s)"
    else
        echo "agent-cleanup: reclaimed $(human "$reclaimed")"
    fi
elif ((quiet)); then
    echo "agent-cleanup: ${#report[@]} path(s) reclaimable"
else
    echo "agent-cleanup: would reclaim $(human "$reclaimed") (pass --apply to delete)"
fi

# Live workspaces are the human's call: their targets are large but deleting
# one costs a rebuild, so report sizes instead of acting.
if ((${#sizes[@]} > 0)) && ((!quiet)); then
    echo
    echo "live workspace targets (not reclaimed):"
    printf '%s\n' "${sizes[@]}"
fi
