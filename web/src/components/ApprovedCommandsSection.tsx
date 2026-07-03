import { useCallback, useEffect, useState } from 'react'
import { authedFetch } from '../store/auth'

/**
 * Settings section listing the shell programs the user has granted a standing
 * "Approve always" to when an agent used the native `run_command` tool. Each
 * approved program runs without re-prompting; revoking one means the next run
 * of that program will ask again.
 */
export default function ApprovedCommandsSection() {
  const [programs, setPrograms] = useState<string[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [revoking, setRevoking] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      const res = await authedFetch('/api/settings/approved-commands')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data = (await res.json()) as { programs: string[] }
      setPrograms(data.programs)
      setError(null)
    } catch {
      setError('Could not load approved commands.')
      setPrograms([])
    }
  }, [])

  // Initial fetch on mount. setState lands only in the async callbacks (not
  // synchronously in the effect), matching the codebase's fetch-in-effect style.
  useEffect(() => {
    let cancelled = false
    authedFetch('/api/settings/approved-commands')
      .then((res) => res.json().then((data) => ({ ok: res.ok, data })))
      .then(({ ok, data }) => {
        if (cancelled) return
        if (!ok) throw new Error('bad status')
        setPrograms((data as { programs: string[] }).programs)
        setError(null)
      })
      .catch(() => {
        if (cancelled) return
        setError('Could not load approved commands.')
        setPrograms([])
      })
    return () => {
      cancelled = true
    }
  }, [])

  const revoke = async (program: string) => {
    setError(null)
    setRevoking(program)
    // Optimistically drop the row; restore by refetch on failure.
    const prev = programs
    setPrograms((list) => (list ? list.filter((p) => p !== program) : list))
    try {
      const res = await authedFetch(
        `/api/settings/approved-commands/${encodeURIComponent(program)}`,
        { method: 'DELETE' },
      )
      if (!res.ok && res.status !== 204) throw new Error(`HTTP ${res.status}`)
    } catch {
      setError(`Could not revoke "${program}".`)
      setPrograms(prev ?? null)
      void load()
    } finally {
      setRevoking(null)
    }
  }

  return (
    <section className="settings-section" data-testid="approved-commands-section">
      <h3>Approved commands</h3>
      <p className="form-hint">
        Programs you allowed with &ldquo;Approve always&rdquo; when an agent asked to run a command.
        These run via the <code>run_command</code> tool without asking again. Revoke one and the
        next run of that program will prompt you.
      </p>

      {error && <p className="settings-error">{error}</p>}

      {programs === null ? (
        <p className="settings-loading">Loading approved commands...</p>
      ) : programs.length === 0 ? (
        <p className="settings-loading">
          No commands have been approved yet. When an agent asks to run a command and you choose
          &ldquo;Approve always&rdquo;, it will appear here.
        </p>
      ) : (
        <ul className="approved-cmd-list" aria-label="Approved commands">
          {programs.map((program) => (
            <li className="approved-cmd-row" key={program} data-testid={`approved-cmd-${program}`}>
              <span className="approved-cmd-name">{program}</span>
              <button
                type="button"
                className="btn-secondary btn-sm"
                onClick={() => void revoke(program)}
                disabled={revoking === program}
                aria-label={`Revoke approval for ${program}`}
                data-testid={`approved-cmd-revoke-${program}`}
              >
                {revoking === program ? 'Revoking…' : 'Revoke'}
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}
