#!/usr/bin/env bash
# Run an agent command inside a resource-capped systemd user scope.
#
# Caps CPU and memory so parallel agents cannot saturate the workstation, and
# optionally serializes GPU access behind a lock. See AGENTS.md, "Resource
# governance".
#
#   scripts/agent-scope.sh -- cargo test -p simulation
#   scripts/agent-scope.sh --cpu-quota 200% -- cargo build
#   scripts/agent-scope.sh --gpu -- cargo run -p voxel-client -- --headless ...
#
# Environment overrides: AGENT_CPU_QUOTA, AGENT_MEMORY_MAX, AGENT_GPU_LOCK.
set -euo pipefail

cpu_quota="${AGENT_CPU_QUOTA:-400%}"
memory_max="${AGENT_MEMORY_MAX:-8G}"
gpu_lock="${AGENT_GPU_LOCK:-${XDG_RUNTIME_DIR:-/tmp}/jtech-agent-gpu.lock}"
gpu=0

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cpu-quota) cpu_quota="$2"; shift 2 ;;
        --memory-max) memory_max="$2"; shift 2 ;;
        --gpu) gpu=1; shift ;;
        -h | --help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *) break ;;
    esac
done

if [[ $# -eq 0 ]]; then
    echo "agent-scope: no command given" >&2
    usage >&2
    exit 2
fi

# Cargo defaults to one job per core; scale it to the quota so a capped scope
# does not spawn more compilers than it can run.
jobs=$((${cpu_quota%\%} / 100))
((jobs < 1)) && jobs=1
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$jobs}"

if ! command -v systemd-run >/dev/null 2>&1; then
    echo "agent-scope: systemd-run unavailable; running uncapped" >&2
    exec "$@"
fi

scope=(
    systemd-run --user --scope --quiet
    -p "CPUQuota=$cpu_quota"
    -p "MemoryMax=$memory_max"
    -p "MemorySwapMax=0"
    --
)

# The GPU is a single shared device: `--gpu` serializes jobs so concurrent
# agents cannot each open a Vulkan context on it. CPU-only work (including
# `--headless --render-backend software`) does not need the lock.
if ((gpu)); then
    exec flock "$gpu_lock" "${scope[@]}" "$@"
fi
exec "${scope[@]}" "$@"
