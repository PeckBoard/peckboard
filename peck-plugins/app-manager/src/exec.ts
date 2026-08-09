// The target abstraction: one interface, two implementations, so every
// catalog entry runs the same way regardless of where it runs.
//   - local:  peckboard_exec_any, via `sh -c <script>`.
//   - remote: the ssh host functions, authenticating with a vault key_id.

import { execAny, sshExec } from "./host";
import { TargetRecord, toConn } from "./targets";

export interface ExecOutcome {
  ok: boolean;
  exitCode: number | null;
  stdout: string;
  stderr: string;
  timedOut: boolean;
  /** Either output stream hit the host's capture cap — the text is incomplete. */
  truncated: boolean;
}

export function runOnTarget(
  target: TargetRecord,
  script: string,
  timeoutSecs?: number,
): ExecOutcome {
  if (target.kind === "local") {
    const res = execAny("sh", ["-c", script], timeoutSecs);
    return {
      ok: res.exit_code === 0,
      exitCode: res.exit_code,
      stdout: res.stdout,
      stderr: res.stderr,
      timedOut: res.timed_out,
      truncated: res.stdout_truncated || res.stderr_truncated,
    };
  }

  const res = sshExec(toConn(target), script, timeoutSecs);
  return {
    ok: res.ok && res.exit_code === 0,
    exitCode: res.exit_code,
    stdout: res.stdout,
    stderr: res.stderr,
    timedOut: res.timed_out,
    truncated: res.stdout_truncated || res.stderr_truncated,
  };
}
