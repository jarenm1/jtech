/**
 * Wrap heavy agent commands in `scripts/agent-scope.sh` so parallel agents
 * cannot saturate the workstation. See AGENTS.md, "Resource governance".
 *
 * The bash tool's command is rewritten when it would compile, test, or run the
 * game without the resource-capped scope. Light commands (`cargo fmt`,
 * `cargo metadata`, `jj`, `rg`) pass through untouched, and a command that
 * already names the wrapper is left alone.
 */
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import type { HookAPI } from "@oh-my-pi/pi-coding-agent/extensibility/hooks";

const WRAPPER = join("scripts", "agent-scope.sh");

// cargo subcommands that compile or run the workspace.
const HEAVY_CARGO =
  /\bcargo\b(?:\s+[^\s;&|]+)*?\s+(?:build|check|test|run|bench|clippy|doc|install|nextest|fix|rustc|b|c|t|r)\b/;
// Game binaries run directly, bypassing cargo.
const GAME_BINARY =
  /(?:^|[\s;&|/])(?:voxel-client|target\/(?:debug|release)\/(?:voxel-client|server))\b/;
// The client opens a Vulkan device unless it renders on the CPU.
const CPU_RENDER = /--render-backend[= ]software\b/;

/** Wrap `command` so it runs inside the scope, adding `--gpu` when needed. */
function wrapCommand(wrapper: string, command: string): string {
  const flags =
    /\bvoxel-client\b/.test(command) && !CPU_RENDER.test(command) ? "--gpu " : "";
  const script = `'${wrapper.replaceAll("'", "'\\''")}'`;
  const body = `'${command.replaceAll("'", "'\\''")}'`;
  return `${script} -- ${flags}bash -c ${body}`;
}

/** Nearest ancestor of `start` that contains the wrapper, if any. */
function findWrapper(start: string): string | undefined {
  let dir = start;
  for (;;) {
    const candidate = join(dir, WRAPPER);
    if (existsSync(candidate)) return candidate;
    const parent = dirname(dir);
    if (parent === dir) return undefined;
    dir = parent;
  }
}

export default function agentScope(pi: HookAPI): void {
  pi.on("tool_call", async (event, ctx) => {
    if (event.toolName !== "bash") return;
    const command = String(event.input.command ?? "");
    if (!command || command.includes("agent-scope.sh")) return;
    if (!HEAVY_CARGO.test(command) && !GAME_BINARY.test(command)) return;

    const wrapper = findWrapper(
      String(event.input.cwd ?? ctx.cwd ?? process.cwd()),
    );
    if (!wrapper) return;

    return { input: { ...event.input, command: wrapCommand(wrapper, command) } };
  });
}
