import { useEffect, useState } from 'react'
import { checkHostCommand, type CommandCheckResult } from '../utils/mcpServers'
import { startInstallSession } from '../utils/installSession'

/**
 * Shared missing-binary chrome (Settings MCP modal and Codex plugin).
 * Reuses `.mcp-cmd-warning` — do not invent a second card/row.
 */
export function MissingCommandWarning({
  command,
  consequence,
  steps,
  installing,
  installMsg,
  onInstall,
  testIdPrefix,
}: {
  command: string
  consequence: string
  steps: string[]
  installing: boolean
  installMsg: string | null
  onInstall: () => void
  testIdPrefix: string
}) {
  return (
    <div className="mcp-cmd-warning" data-testid={`${testIdPrefix}-cmd-warning`}>
      <div className="mcp-cmd-warning-head">
        <code>{command}</code> was not found on the Peckboard host&apos;s PATH. {consequence}
      </div>
      {steps.length > 0 ? (
        <ul className="mcp-cmd-warning-steps">
          {steps.map((s, i) => (
            <li key={i}>{s}</li>
          ))}
        </ul>
      ) : null}
      <div className="mcp-cmd-warning-actions">
        <button
          type="button"
          className="mcp-btn mcp-btn--primary"
          disabled={installing}
          data-testid={`${testIdPrefix}-install-in-session`}
          onClick={onInstall}
        >
          {installing ? 'Opening…' : 'Install in a session'}
        </button>
        <span className="mcp-cmd-warning-hint">
          Opens a temporary session that installs it for you (sudo prompts appear here).
        </span>
      </div>
      {installMsg && <div className="mcp-cmd-warning-msg">{installMsg}</div>}
    </div>
  )
}

/** Debounced PATH probe + install-in-session for a provider CLI (Codex). */
export function HostCliMissingBanner({
  command,
  requiredBy,
  doneHint,
  consequence,
  testIdPrefix,
}: {
  command: string
  requiredBy: string
  doneHint: string
  consequence: string
  testIdPrefix: string
}) {
  const cmd = command.trim()
  const [check, setCheck] = useState<{ command: string; result: CommandCheckResult } | null>(null)
  const [installing, setInstalling] = useState(false)
  const [installMsg, setInstallMsg] = useState<string | null>(null)

  useEffect(() => {
    if (!cmd) return
    let cancelled = false
    const t = setTimeout(() => {
      void checkHostCommand(cmd).then((r) => {
        if (!cancelled && r) setCheck({ command: cmd, result: r })
      })
    }, 400)
    return () => {
      cancelled = true
      clearTimeout(t)
    }
  }, [cmd])

  const result = check?.command === cmd ? check.result : null
  if (!cmd || !result || result.found !== false) return null

  const runInstall = async () => {
    setInstalling(true)
    setInstallMsg(null)
    try {
      await startInstallSession({
        command: cmd,
        requiredBy,
        steps: result.hints,
        suggestedFolderPath: result.suggested_folder_path,
        doneHint,
      })
      setInstallMsg('Install session opened — watch its tab, then re-check this page.')
    } catch (e) {
      setInstallMsg(e instanceof Error ? e.message : 'Could not start install session.')
    } finally {
      setInstalling(false)
    }
  }

  return (
    <MissingCommandWarning
      command={cmd}
      consequence={consequence}
      steps={result.hints}
      installing={installing}
      installMsg={installMsg}
      onInstall={() => void runInstall()}
      testIdPrefix={testIdPrefix}
    />
  )
}
