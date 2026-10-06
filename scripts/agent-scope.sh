#!/usr/bin/env bash
# Run an agent command inside a resource-capped systemd user scope.
#
# Caps CPU and memory so parallel agents cannot saturate the workstation, and
# optionally serializes GPU access behind a lock. Every scope also joins one
# shared slice whose aggregate cap bounds the total across all agents, not just
# each scope. See AGENTS.md, "Resource governance".
#
#   scripts/agent-scope.sh -- cargo test -p simulation
#   scripts/agent-scope.sh --cpu-quota 200% -- cargo build
#   scripts/agent-scope.sh --gpu -- cargo run -p voxel-client -- --headless ...
#
# The per-scope quota is a burst ceiling: one agent alone may use the whole
# machine. The slice quota is the aggregate ceiling that actually bounds N
# concurrent agents, so it is the number to tune for agent count.
#
# Environment overrides: AGENT_CPU_QUOTA, AGENT_MEMORY_MAX, AGENT_CARGO_JOBS,
# AGENT_GPU_LOCK, AGENT_SLICE, AGENT_SLICE_CPU_QUOTA, AGENT_SLICE_MEMORY_MAX.
#
# The slice caps are owned by this script: every invocation re-applies them
# from the values below, so a systemd drop-in is not the tuning mechanism —
# set AGENT_SLICE_CPU_QUOTA / AGENT_SLICE_MEMORY_MAX instead.
set -euo pipefail

# Per-scope burst ceiling. 800% lets one agent use all 8 cores when it is the
# only one running; the slice quota below is what bounds the aggregate.
cpu_quota="${AGENT_CPU_QUOTA:-800%}"
# One cold `cargo build --workspace` peaks near 2.8G, so 8G leaves headroom for
# a link plus a test run without letting one agent eat the slice.
memory_max="${AGENT_MEMORY_MAX:-8G}"
# Aggregate ceiling across every agent. 700% leaves a core for the human's
# editor and rust-analyzer; raise to 800% to hand agents the whole machine.
slice_cpu_quota="${AGENT_SLICE_CPU_QUOTA:-700%}"
# 20G of 31G total: six concurrent cold builds peak near 17G, and the human's
# session already holds ~8G.
slice_memory_max="${AGENT_SLICE_MEMORY_MAX:-20G}"
# Cargo jobs per agent. Left empty, it scales to how many agents are actually
# running: one agent alone gets the whole machine, six agents each get a slice
# of it. Memory is the binding constraint, not CPU — six agents at -j8 would
# spawn 48 rustc processes and blow the slice memory cap, while the slice CPU
# quota already throttles throughput under contention.
cargo_jobs="${AGENT_CARGO_JOBS:-}"
gpu_lock="${AGENT_GPU_LOCK:-${XDG_RUNTIME_DIR:-/tmp}/jtech-agent-gpu.lock}"
slice="${AGENT_SLICE:-agents.slice}"
gpu=0

usage() {
    sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cpu-quota) cpu_quota="$2"; shift 2 ;;
        --memory-max) memory_max="$2"; shift 2 ;;
        --jobs) cargo_jobs="$2"; shift 2 ;;
        --slice) slice="$2"; shift ;;
        --no-slice) slice=""; shift ;;
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

if [[ -z "$cargo_jobs" ]]; then
    cores=$(nproc 2>/dev/null || echo 4)
    active=1
    if [[ -n "$slice" ]] && command -v systemctl >/dev/null 2>&1; then
        cg=$(systemctl --user show "$slice" -p ControlGroup --value 2>/dev/null || true)
        if [[ -n "$cg" && -d "/sys/fs/cgroup$cg" ]]; then
            # One directory per live agent scope; this invocation is not one yet.
            running=$({ ls -d "/sys/fs/cgroup$cg"/*.scope 2>/dev/null || true; } | wc -l)
            active=$((running + 1))
        fi
    fi
    cargo_jobs=$((cores / active))
    ((cargo_jobs < 2)) && cargo_jobs=2
    ((cargo_jobs > cores)) && cargo_jobs=$cores
fi

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$cargo_jobs}"

if ! command -v systemd-run >/dev/null 2>&1; then
    echo "agent-scope: systemd-run unavailable; running uncapped" >&2
    exec "$@"
fi

scope=(
    systemd-run --user --scope --quiet
    -p "CPUQuota=$cpu_quota"
    -p "MemoryMax=$memory_max"
    -p "MemorySwapMax=0"
)

# Every agent scope joins one shared slice so the aggregate across parallel
# agents is bounded, not just each scope. The slice is created on demand and
# its caps are re-applied on every invocation: this script is the source of
# truth, so tuning happens through the AGENT_SLICE_* variables above.
if [[ -n "$slice" ]]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user start "$slice" >/dev/null 2>&1 || true
        systemctl --user set-property "$slice" \
            "CPUQuota=$slice_cpu_quota" \
            "MemoryMax=$slice_memory_max" \
            >/dev/null 2>&1 || true
    fi
    scope+=("--slice=$slice")
fi

scope+=(--)

# The GPU is a single shared device: `--gpu` serializes jobs so concurrent
# agents cannot each open a Vulkan context on it. CPU-only work (including
# `--headless --render-backend software`) does not need the lock.
if ((gpu)); then
    exec flock "$gpu_lock" "${scope[@]}" "$@"
fi
exec "${scope[@]}" "$@"
