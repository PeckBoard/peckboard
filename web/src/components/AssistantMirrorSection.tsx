import { useCallback, useEffect, useState } from 'react'
import { formatRelativeTime } from '../lib/review'
import { authedFetch } from '../store/auth'
import FieldError from './FieldError'
import SecretInput from './SecretInput'

type Channel = 'slack' | 'discord' | 'email'
type Tls = 'starttls' | 'tls' | 'none'

interface Status {
  state: 'ok' | 'error' | 'never'
  at?: string | null
  error?: string | null
}

/** `GET /api/assistant/mirror` (`routes/assistant.rs`). Secrets come back
 *  only as `webhook_set` / `password_set`. */
interface MirrorConfig {
  redact_code_blocks: boolean
  slack: { enabled: boolean; webhook_set: boolean; status: Status }
  discord: { enabled: boolean; webhook_set: boolean; status: Status }
  email: { enabled: boolean; to: string; status: Status }
  smtp: {
    host: string
    port: number
    tls: Tls
    username: string
    password_set: boolean
    from: string
  }
  watched: boolean
}

/** A write-only secret: `value` replaces it, `clear` empties it, neither keeps it. */
interface SecretDraft {
  value: string
  clear: boolean
}

interface Draft {
  redact: boolean
  slack: { enabled: boolean } & SecretDraft
  discord: { enabled: boolean } & SecretDraft
  email: { enabled: boolean; to: string }
  smtp: {
    host: string
    port: string
    tls: Tls
    username: string
    password: SecretDraft
    from: string
  }
}

/** Server field path (`slack.webhook_url`) → message. */
type Errors = Partial<Record<string, string>>

const POLL_MS = 10_000
const NO_SECRET: SecretDraft = { value: '', clear: false }
const TLS_OPTIONS: { id: Tls; label: string }[] = [
  { id: 'starttls', label: 'STARTTLS' },
  { id: 'tls', label: 'TLS' },
  { id: 'none', label: 'None (localhost only)' },
]
const WEBHOOKS: { id: 'slack' | 'discord'; title: string; placeholder: string }[] = [
  { id: 'slack', title: 'Slack', placeholder: 'https://hooks.slack.com/services/…' },
  { id: 'discord', title: 'Discord', placeholder: 'https://discord.com/api/webhooks/…' },
]

function draftFrom(c: MirrorConfig): Draft {
  return {
    redact: c.redact_code_blocks,
    slack: { enabled: c.slack.enabled, ...NO_SECRET },
    discord: { enabled: c.discord.enabled, ...NO_SECRET },
    email: { enabled: c.email.enabled, to: c.email.to },
    smtp: {
      host: c.smtp.host,
      port: String(c.smtp.port),
      tls: c.smtp.tls,
      username: c.smtp.username,
      password: NO_SECRET,
      from: c.smtp.from,
    },
  }
}

/** Whether a secret will be set after saving `d` over `isSet`. */
const secretAfter = (isSet: boolean, d: SecretDraft) => d.value.trim() !== '' || (isSet && !d.clear)

/** The checks the UI can make before asking the server (it re-checks all). */
function validate(c: MirrorConfig, d: Draft): Errors {
  const e: Errors = {}
  for (const w of WEBHOOKS) {
    if (d[w.id].enabled && !secretAfter(c[w.id].webhook_set, d[w.id])) {
      e[`${w.id}.webhook_url`] = `Add a webhook URL to turn ${w.title} on`
    }
  }
  const port = Number(d.smtp.port.trim())
  if (!/^\d+$/.test(d.smtp.port.trim()) || port < 1 || port > 65535) {
    e['smtp.port'] = 'Port must be 1–65535'
  }
  if (d.email.enabled) {
    if (!d.email.to.trim()) e['email.to'] = 'Add a recipient to turn email on'
    if (!d.smtp.host.trim()) e['smtp.host'] = 'Required for email'
    if (!d.smtp.from.trim()) e['smtp.from'] = 'Required for email'
  }
  return e
}

function secretPatch(d: SecretDraft): string | undefined {
  if (d.value.trim() !== '') return d.value.trim()
  return d.clear ? '' : undefined
}

function patchFrom(d: Draft) {
  return {
    redact_code_blocks: d.redact,
    slack: { enabled: d.slack.enabled, webhook_url: secretPatch(d.slack) },
    discord: { enabled: d.discord.enabled, webhook_url: secretPatch(d.discord) },
    email: { enabled: d.email.enabled, to: d.email.to.trim() },
    smtp: {
      host: d.smtp.host.trim(),
      port: Number(d.smtp.port.trim()),
      tls: d.smtp.tls,
      username: d.smtp.username.trim(),
      password: secretPatch(d.smtp.password),
      from: d.smtp.from.trim(),
    },
  }
}

function StatusLine({ status, testId }: { status: Status; testId: string }) {
  const when = status.at ? ` · ${formatRelativeTime(status.at)}` : ''
  return (
    <span
      className={`mirror-status mirror-status-${status.state}`}
      data-testid={testId}
      data-state={status.state}
      role="status"
    >
      {status.state === 'never'
        ? 'Nothing sent yet'
        : status.state === 'ok'
          ? `Delivered${when}`
          : `Failed${when}${status.error ? ` — ${status.error}` : ''}`}
    </span>
  )
}

/**
 * Settings → Assistant → Conversation Mirror: copies what you say to the
 * Assistant and its replies to Slack / Discord webhooks, and emails a digest
 * of turns that finish while no device is watching the Assistant panel.
 * Admin-only, like the API behind it (`/api/assistant/mirror`).
 *
 * One Save for the whole form; Send test uses the saved settings, so it waits
 * for unsaved edits to be saved. Secrets are write-only: the inputs start
 * empty and say "•••• saved" when a value is stored.
 */
export default function AssistantMirrorSection() {
  const [config, setConfig] = useState<MirrorConfig | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [serverErrors, setServerErrors] = useState<Errors>({})
  const [error, setError] = useState('')
  const [saving, setSaving] = useState(false)
  const [testing, setTesting] = useState<Channel | null>(null)
  const [savedNote, setSavedNote] = useState(false)

  const load = useCallback(
    () =>
      authedFetch('/api/assistant/mirror')
        .then((res) => {
          if (!res.ok) throw new Error(`HTTP ${res.status}`)
          return res.json() as Promise<MirrorConfig>
        })
        .then(
          (c) => {
            setConfig(c)
            return c
          },
          (e: unknown) => {
            setError(
              `Couldn't load the mirror settings: ${e instanceof Error ? e.message : String(e)}`,
            )
            return null
          },
        ),
    [],
  )

  useEffect(() => {
    void load().then((c) => {
      if (c) setDraft(draftFrom(c))
    })
    const t = setInterval(() => void load(), POLL_MS)
    return () => clearInterval(t)
  }, [load])

  if (!config || !draft) {
    return (
      <section
        className="settings-section"
        data-testid="assistant-mirror-section"
        data-settings-anchor="assistant-mirror"
      >
        <h3>Conversation Mirror</h3>
        {error ? (
          <p className="form-error" role="alert">
            {error}
          </p>
        ) : (
          <p className="form-hint">Loading…</p>
        )}
      </section>
    )
  }

  const clientErrors = validate(config, draft)
  const errors: Errors = { ...serverErrors, ...clientErrors }
  const invalid = Object.keys(clientErrors).length > 0
  const dirty = JSON.stringify(draft) !== JSON.stringify(draftFrom(config))

  /** Apply an edit and drop the server errors for the fields it touched. */
  const edit = (fields: string[], next: (d: Draft) => Draft) => {
    setDraft((d) => (d ? next(d) : d))
    setSavedNote(false)
    setServerErrors((errs) => {
      const rest = { ...errs }
      for (const f of fields) delete rest[f]
      return rest
    })
  }

  const save = async () => {
    setSaving(true)
    setError('')
    try {
      const res = await authedFetch('/api/assistant/mirror', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(patchFrom(draft)),
      })
      const body = (await res.json().catch(() => ({}))) as
        | MirrorConfig
        | { errors?: Errors; error?: string }
      if (!res.ok) {
        if ('errors' in body && body.errors) setServerErrors(body.errors)
        else setError(('error' in body && body.error) || `Save failed (HTTP ${res.status})`)
        return
      }
      const c = body as MirrorConfig
      setConfig(c)
      setDraft(draftFrom(c))
      setServerErrors({})
      setSavedNote(true)
    } catch (e) {
      setError(`Save failed: ${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setSaving(false)
    }
  }

  const sendTest = async (channel: Channel) => {
    setTesting(channel)
    setError('')
    try {
      const res = await authedFetch('/api/assistant/mirror/test', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ channel }),
      })
      if (!res.ok) {
        const body = (await res.json().catch(() => ({}))) as { error?: string }
        setError(body.error ?? `Test failed (HTTP ${res.status})`)
      }
      // The outcome lands in the channel's status line.
      await load()
    } finally {
      setTesting(null)
    }
  }

  const saveReason = saving
    ? null
    : invalid
      ? 'Fix the fields above to save.'
      : !dirty
        ? savedNote
          ? null
          : 'No changes to save.'
        : null
  const testReason = dirty ? 'Save your changes before sending a test.' : null

  const testButton = (channel: Channel, ready: boolean, missing: string) => (
    <div className="mirror-test-row">
      <button
        type="button"
        className="btn-secondary btn-sm"
        disabled={dirty || !ready || testing !== null}
        onClick={() => void sendTest(channel)}
        data-testid={`mirror-${channel}-test`}
      >
        {testing === channel ? 'Sending…' : 'Send test'}
      </button>
      {(testReason || !ready) && (
        <span className="form-actions-reason" data-testid={`mirror-${channel}-test-reason`}>
          {testReason ?? missing}
        </span>
      )}
      <StatusLine status={config[channel].status} testId={`mirror-${channel}-status`} />
    </div>
  )

  return (
    <section
      className="settings-section mirror-section"
      data-testid="assistant-mirror-section"
      data-settings-anchor="assistant-mirror"
    >
      <div className="settings-section-head">
        <h3>Conversation Mirror</h3>
        <span
          className={`mirror-watched${config.watched ? ' watched' : ''}`}
          data-testid="mirror-watched"
          data-watched={config.watched ? 'true' : 'false'}
          title={
            config.watched
              ? 'The Assistant panel is open on a device: no email digest is collected'
              : 'No device has the Assistant panel open: turns go to the email digest'
          }
        >
          {config.watched ? 'Watched now' : 'Not watched'}
        </span>
      </div>
      <p className="form-hint">
        Mirrors what you say and the Assistant&apos;s replies — never its thinking or tool output.
        Secrets are masked on a best-effort basis, and Slack, Discord and your mail provider will
        store the transcript.
      </p>

      {WEBHOOKS.map((w) => {
        const d = draft[w.id]
        const isSet = config[w.id].webhook_set
        const field = `${w.id}.webhook_url`
        return (
          <div className="settings-subsection" key={w.id} data-testid={`mirror-${w.id}`}>
            <h4>{w.title}</h4>
            <label className="settings-row settings-row-toggle">
              <input
                type="checkbox"
                checked={d.enabled}
                onChange={(e) =>
                  edit([field], (x) => ({
                    ...x,
                    [w.id]: { ...x[w.id], enabled: e.target.checked },
                  }))
                }
                data-testid={`mirror-${w.id}-enabled`}
              />
              <span className="settings-label">Post the conversation to {w.title}</span>
            </label>
            <div className="form-field">
              <label className="form-label" htmlFor={`mirror-${w.id}-url`}>
                Webhook URL
              </label>
              <div className="settings-row">
                <SecretInput
                  id={`mirror-${w.id}-url`}
                  className="form-input"
                  label={`${w.title} webhook URL`}
                  value={d.value}
                  placeholder={d.clear ? 'Cleared on save' : isSet ? '•••• saved' : w.placeholder}
                  onChange={(v) =>
                    edit([field], (x) => ({ ...x, [w.id]: { ...x[w.id], value: v, clear: false } }))
                  }
                  testId={`mirror-${w.id}-url`}
                  revealTestId={`mirror-${w.id}-url-reveal`}
                />
                {isSet && (
                  <button
                    type="button"
                    className="btn-secondary btn-sm"
                    onClick={() =>
                      edit([field], (x) => ({
                        ...x,
                        [w.id]: { ...x[w.id], value: '', clear: !x[w.id].clear },
                      }))
                    }
                    data-testid={`mirror-${w.id}-clear`}
                  >
                    {d.clear ? 'Keep' : 'Clear'}
                  </button>
                )}
              </div>
              <FieldError message={errors[field]} testId={`mirror-${w.id}-url-error`} />
            </div>
            {testButton(w.id, isSet, 'Save a webhook URL to send a test.')}
          </div>
        )
      })}

      <div className="settings-subsection" data-testid="mirror-email">
        <h4>Email When Not Watching</h4>
        <label className="settings-row settings-row-toggle">
          <input
            type="checkbox"
            checked={draft.email.enabled}
            onChange={(e) =>
              edit(['email.to', 'smtp.host', 'smtp.from'], (x) => ({
                ...x,
                email: { ...x.email, enabled: e.target.checked },
              }))
            }
            data-testid="mirror-email-enabled"
          />
          <span className="settings-label">
            Email a digest of turns that finish while no device has the Assistant open
          </span>
        </label>
        <div className="form-field">
          <label className="form-label" htmlFor="mirror-email-to">
            Recipient
          </label>
          <input
            id="mirror-email-to"
            className="form-input"
            type="email"
            autoComplete="off"
            placeholder="you@example.com"
            value={draft.email.to}
            onChange={(e) =>
              edit(['email.to'], (x) => ({ ...x, email: { ...x.email, to: e.target.value } }))
            }
            data-testid="mirror-email-to"
          />
          <FieldError message={errors['email.to']} testId="mirror-email-to-error" />
        </div>
        {testButton(
          'email',
          config.email.to !== '' && config.smtp.host !== '' && config.smtp.from !== '',
          'Save a recipient, SMTP host and From address to send a test.',
        )}
      </div>

      <div className="settings-subsection" data-testid="mirror-smtp">
        <h4>SMTP</h4>
        <div className="form-row">
          <div className="form-field">
            <label className="form-label" htmlFor="mirror-smtp-host">
              Host
            </label>
            <input
              id="mirror-smtp-host"
              className="form-input"
              type="text"
              autoComplete="off"
              placeholder="smtp.example.com"
              value={draft.smtp.host}
              onChange={(e) =>
                edit(['smtp.host', 'smtp.tls'], (x) => ({
                  ...x,
                  smtp: { ...x.smtp, host: e.target.value },
                }))
              }
              data-testid="mirror-smtp-host"
            />
            <FieldError message={errors['smtp.host']} testId="mirror-smtp-host-error" />
          </div>
          <div className="form-field mirror-port-field">
            <label className="form-label" htmlFor="mirror-smtp-port">
              Port
            </label>
            <input
              id="mirror-smtp-port"
              className="form-input"
              type="number"
              inputMode="numeric"
              min={1}
              max={65535}
              value={draft.smtp.port}
              onChange={(e) =>
                edit(['smtp.port'], (x) => ({ ...x, smtp: { ...x.smtp, port: e.target.value } }))
              }
              data-testid="mirror-smtp-port"
            />
            <FieldError message={errors['smtp.port']} testId="mirror-smtp-port-error" />
          </div>
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="mirror-smtp-tls">
            Encryption
          </label>
          <select
            id="mirror-smtp-tls"
            className="form-input"
            value={draft.smtp.tls}
            onChange={(e) =>
              edit(['smtp.tls'], (x) => ({
                ...x,
                smtp: { ...x.smtp, tls: e.target.value as Tls },
              }))
            }
            data-testid="mirror-smtp-tls"
          >
            {TLS_OPTIONS.map((o) => (
              <option key={o.id} value={o.id}>
                {o.label}
              </option>
            ))}
          </select>
          <FieldError message={errors['smtp.tls']} testId="mirror-smtp-tls-error" />
        </div>
        <div className="form-row">
          <div className="form-field">
            <label className="form-label" htmlFor="mirror-smtp-username">
              Username
            </label>
            <input
              id="mirror-smtp-username"
              className="form-input"
              type="text"
              autoComplete="off"
              value={draft.smtp.username}
              onChange={(e) =>
                edit(['smtp.username'], (x) => ({
                  ...x,
                  smtp: { ...x.smtp, username: e.target.value },
                }))
              }
              data-testid="mirror-smtp-username"
            />
            <FieldError message={errors['smtp.username']} testId="mirror-smtp-username-error" />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="mirror-smtp-password">
              Password
            </label>
            <div className="settings-row">
              <SecretInput
                id="mirror-smtp-password"
                className="form-input"
                label="SMTP password"
                value={draft.smtp.password.value}
                placeholder={
                  draft.smtp.password.clear
                    ? 'Cleared on save'
                    : config.smtp.password_set
                      ? '•••• saved'
                      : ''
                }
                onChange={(v) =>
                  edit(['smtp.username'], (x) => ({
                    ...x,
                    smtp: { ...x.smtp, password: { value: v, clear: false } },
                  }))
                }
                testId="mirror-smtp-password"
                revealTestId="mirror-smtp-password-reveal"
              />
              {config.smtp.password_set && (
                <button
                  type="button"
                  className="btn-secondary btn-sm"
                  onClick={() =>
                    edit(['smtp.username'], (x) => ({
                      ...x,
                      smtp: { ...x.smtp, password: { value: '', clear: !x.smtp.password.clear } },
                    }))
                  }
                  data-testid="mirror-smtp-password-clear"
                >
                  {draft.smtp.password.clear ? 'Keep' : 'Clear'}
                </button>
              )}
            </div>
          </div>
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="mirror-smtp-from">
            From address
          </label>
          <input
            id="mirror-smtp-from"
            className="form-input"
            type="email"
            autoComplete="off"
            placeholder="peckboard@example.com"
            value={draft.smtp.from}
            onChange={(e) =>
              edit(['smtp.from'], (x) => ({ ...x, smtp: { ...x.smtp, from: e.target.value } }))
            }
            data-testid="mirror-smtp-from"
          />
          <FieldError message={errors['smtp.from']} testId="mirror-smtp-from-error" />
        </div>
      </div>

      <label className="settings-row settings-row-toggle mirror-redact">
        <input
          type="checkbox"
          checked={draft.redact}
          onChange={(e) => edit([], (x) => ({ ...x, redact: e.target.checked }))}
          data-testid="mirror-redact-code"
        />
        <span className="settings-label">Redact code blocks</span>
      </label>
      <p className="form-hint mirror-redact-hint">
        Replaces code blocks and indented output with &ldquo;[code omitted]&rdquo; before sending.
      </p>

      {error && (
        <p className="form-error" role="alert" data-testid="mirror-error">
          {error}
        </p>
      )}
      <div className="form-actions">
        {saveReason && (
          <span className="form-actions-reason" data-testid="mirror-save-reason">
            {saveReason}
          </span>
        )}
        {savedNote && !dirty && <span className="form-hint">Saved.</span>}
        <button
          type="button"
          className="btn-primary btn-sm"
          disabled={saving || invalid || !dirty}
          onClick={() => void save()}
          data-testid="mirror-save"
        >
          {saving ? 'Saving…' : 'Save'}
        </button>
      </div>
    </section>
  )
}
