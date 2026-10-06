/**
 * Reclaim disk from abandoned agent workspaces when a session ends.
 *
 * Every workspace keeps its own target dir, and a full one runs 10-35G, so
 * forgotten workspaces are the main disk consumer on this machine. This hook
 * fires the same sweep the agent is told to run in AGENTS.md, "Reclaiming
 * disk", so cleanup happens even when an agent forgets.
 *
 * It is deliberately conservative: `scripts/agent-cleanup.sh --apply` moves a
 * real in-tree `target/` out to `~/workspaces/.targets/<slug>` (a rename, so
 * nothing is lost), deletes target dirs for workspaces the human has already
 * forgotten, and deletes `incremental`/`examples` for workspaces idle longer
 * than a week. `deps` and built binaries of a live workspace are never
 * touched, so the next warm build stays fast. The slow `nix store gc` pass is
 * left to the manual `--gc` flag.
 *
 * The sweep runs detached: the session is shutting down, so nothing waits on
 * it, and a failure must not affect the exit path.
 */
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import type { HookAPI } from "@oh-my-pi/pi-coding-agent/extensibility/hooks";

export default function agentCleanup(pi: HookAPI): void {
  pi.on("session_shutdown", async (_event, ctx) => {
    const script = join(ctx.cwd, "scripts", "agent-cleanup.sh");
    if (!existsSync(script)) return;

    try {
      const child = spawn(script, ["--apply", "--quiet"], {
        cwd: ctx.cwd,
        detached: true,
        stdio: "ignore",
      });
      child.unref();
    } catch (error) {
      pi.logger?.warn?.(`agent-cleanup hook: ${String(error)}`);
    }
  });
}
