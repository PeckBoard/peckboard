import { useState, type FormEvent } from 'react'
import Modal from '../Modal'
import FieldError from '../FieldError'
import { CopyButton } from '../ToolUseBlock'
import {
  createInvite,
  createTeam,
  formatWhen,
  joinTeam,
  nameError,
  previewInvite,
  type InvitePreview,
  type Team,
  type TeamInvite,
} from '../../store/teams'

const plural = (n: number, one: string, many: string) => `${n} ${n === 1 ? one : many}`

/** Create a team of servers; this server becomes its owner. */
export function CreateTeamModal({
  defaultLabel,
  onClose,
  onCreated,
}: {
  defaultLabel: string
  onClose: () => void
  onCreated: (team: Team) => void
}) {
  const [name, setName] = useState('')
  const [label, setLabel] = useState(defaultLabel)
  const [touched, setTouched] = useState({ name: false, label: false })
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const nameErr = nameError(name, 'a team name')
  const labelErr = nameError(label, 'a name for this server')
  const reason = nameErr || labelErr

  const submit = (e: FormEvent) => {
    e.preventDefault()
    setTouched({ name: true, label: true })
    if (reason) return
    setBusy(true)
    setError(null)
    createTeam(name.trim(), label.trim())
      .then(onCreated)
      .catch((err: Error) => setError(err.message))
      .finally(() => setBusy(false))
  }

  return (
    <Modal onClose={onClose} maxWidth={480} data-testid="team-create-modal">
      <h2>Create a team</h2>
      <p className="form-hint">
        This server becomes the team&rsquo;s owner. You can then invite your coworkers&rsquo;
        servers.
      </p>
      <form onSubmit={submit}>
        <div className="form-field">
          <label className="form-label" htmlFor="team-create-name">
            Team name
          </label>
          <input
            id="team-create-name"
            className="form-input"
            value={name}
            maxLength={64}
            onChange={(e) => setName(e.target.value)}
            onBlur={() => setTouched((t) => ({ ...t, name: true }))}
            placeholder="Design team"
            data-testid="team-create-name"
            autoFocus
          />
          <FieldError message={touched.name ? nameErr : ''} testId="team-create-name-error" />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="team-create-label">
            This server&rsquo;s name
          </label>
          <input
            id="team-create-label"
            className="form-input"
            value={label}
            maxLength={64}
            onChange={(e) => setLabel(e.target.value)}
            onBlur={() => setTouched((t) => ({ ...t, label: true }))}
            data-testid="team-create-label"
          />
          <p className="form-hint">How this server appears in the team&rsquo;s member list.</p>
          <FieldError message={touched.label ? labelErr : ''} testId="team-create-label-error" />
        </div>
        {error && (
          <p className="form-error" role="alert" data-testid="team-create-error">
            {error}
          </p>
        )}
        <div className="form-actions">
          {reason && (
            <span className="form-actions-reason" data-testid="team-create-reason">
              {reason}
            </span>
          )}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!reason}
            data-testid="team-create-submit"
          >
            {busy ? 'Creating…' : 'Create team'}
          </button>
        </div>
      </form>
    </Modal>
  )
}

/** Paste an invite link → see which team it is → ask to join. */
export function JoinTeamModal({
  defaultLabel,
  onClose,
  onJoined,
}: {
  defaultLabel: string
  onClose: () => void
  onJoined: (team: Team) => void
}) {
  const [link, setLink] = useState('')
  const [preview, setPreview] = useState<InvitePreview | null>(null)
  const [label, setLabel] = useState(defaultLabel)
  const [labelTouched, setLabelTouched] = useState(false)
  const [busy, setBusy] = useState(false)
  const [linkError, setLinkError] = useState('')
  const [error, setError] = useState<string | null>(null)

  const labelErr = nameError(label, 'a name for this server')

  const check = (e: FormEvent) => {
    e.preventDefault()
    if (!link.trim()) return
    setBusy(true)
    setLinkError('')
    previewInvite(link.trim())
      .then(setPreview)
      .catch((err: Error) => setLinkError(err.message))
      .finally(() => setBusy(false))
  }

  const join = (e: FormEvent) => {
    e.preventDefault()
    setLabelTouched(true)
    if (labelErr) return
    setBusy(true)
    setError(null)
    joinTeam(link.trim(), label.trim())
      .then(onJoined)
      .catch((err: Error) => setError(err.message))
      .finally(() => setBusy(false))
  }

  return (
    <Modal onClose={onClose} maxWidth={520} data-testid="team-join-modal">
      <h2>Join a team</h2>
      {!preview ? (
        <form onSubmit={check}>
          <div className="form-field">
            <label className="form-label" htmlFor="team-join-link">
              Invite link
            </label>
            <textarea
              id="team-join-link"
              className="form-input team-join-link"
              rows={3}
              value={link}
              onChange={(e) => {
                setLink(e.target.value)
                if (linkError) setLinkError('')
              }}
              placeholder="Paste the link your coworker sent you"
              spellCheck={false}
              data-testid="team-join-link"
              autoFocus
            />
            <FieldError message={linkError} testId="team-join-link-error" />
          </div>
          <div className="form-actions">
            {!link.trim() && (
              <span className="form-actions-reason">Paste the invite link first</span>
            )}
            <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
              Cancel
            </button>
            <button
              type="submit"
              className="btn-primary"
              disabled={busy || !link.trim()}
              data-testid="team-join-continue"
            >
              {busy ? 'Checking…' : 'Continue'}
            </button>
          </div>
        </form>
      ) : (
        <form onSubmit={join}>
          <div className="team-join-preview" data-testid="team-join-preview">
            <span className="team-join-preview-name" data-testid="team-join-preview-name">
              {preview.name}
            </span>
            <span className="form-hint">
              {plural(Number(preview.members) || 0, 'server', 'servers')} in this team
            </span>
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="team-join-label">
              This server&rsquo;s name
            </label>
            <input
              id="team-join-label"
              className="form-input"
              value={label}
              maxLength={64}
              onChange={(e) => setLabel(e.target.value)}
              onBlur={() => setLabelTouched(true)}
              data-testid="team-join-label"
            />
            <p className="form-hint">How this server appears in the team&rsquo;s member list.</p>
            <FieldError message={labelTouched ? labelErr : ''} testId="team-join-label-error" />
          </div>
          <p className="form-hint">
            After you ask, this page shows a six-digit code. Read it to the person who invited you
            so they can approve the request.
          </p>
          {error && (
            <p className="form-error" role="alert" data-testid="team-join-error">
              {error}
            </p>
          )}
          <div className="form-actions">
            {labelErr && <span className="form-actions-reason">{labelErr}</span>}
            <button
              type="button"
              className="btn-secondary"
              onClick={() => setPreview(null)}
              disabled={busy}
            >
              Back
            </button>
            <button
              type="submit"
              className="btn-primary"
              disabled={busy || !!labelErr}
              data-testid="team-join-submit"
            >
              {busy ? 'Asking…' : 'Ask to join'}
            </button>
          </div>
        </form>
      )}
    </Modal>
  )
}

const DAY_CHOICES = [1, 2, 3, 4, 5, 6, 7]

/** Make a single-use invite link for one coworker's server. */
export function InviteModal({
  teamId,
  teamName,
  onClose,
  onCreated,
}: {
  teamId: string
  teamName: string
  onClose: () => void
  onCreated: (invite: TeamInvite) => void
}) {
  const [days, setDays] = useState(3)
  const [invite, setInvite] = useState<TeamInvite | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const make = () => {
    setBusy(true)
    setError(null)
    createInvite(teamId, days)
      .then((inv) => {
        setInvite(inv)
        onCreated(inv)
      })
      .catch((err: Error) => setError(err.message))
      .finally(() => setBusy(false))
  }

  return (
    <Modal onClose={onClose} maxWidth={540} data-testid="team-invite-modal">
      <h2>Invite a server to {teamName}</h2>
      {!invite ? (
        <>
          <div className="form-field">
            <label className="form-label" htmlFor="team-invite-days">
              Link works for
            </label>
            <select
              id="team-invite-days"
              className="form-input"
              value={days}
              onChange={(e) => setDays(Number(e.target.value))}
              data-testid="team-invite-days"
            >
              {DAY_CHOICES.map((d) => (
                <option key={d} value={d}>
                  {plural(d, 'day', 'days')}
                </option>
              ))}
            </select>
          </div>
          {error && (
            <p className="form-error" role="alert" data-testid="team-invite-error">
              {error}
            </p>
          )}
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
              Cancel
            </button>
            <button
              type="button"
              className="btn-primary"
              onClick={make}
              disabled={busy}
              data-testid="team-invite-create"
            >
              {busy ? 'Creating…' : 'Create invite link'}
            </button>
          </div>
        </>
      ) : (
        <>
          <p className="form-hint">
            Send this link to one coworker by chat or email. It works once.
          </p>
          <div className="team-invite-link-row">
            <input
              className="form-input team-invite-link"
              readOnly
              value={invite.link}
              onFocus={(e) => e.currentTarget.select()}
              aria-label="Invite link"
              data-testid="team-invite-link"
            />
            <CopyButton text={invite.link} className="btn-secondary btn-sm" label="Copy link" />
          </div>
          <p className="form-hint" data-testid="team-invite-expiry">
            Expires {formatWhen(invite.expires_at)}.
          </p>
          <p className="form-hint">
            When they ask to join, a request with a six-digit code appears on this page. Approve it
            only after they read you the same code.
          </p>
          <div className="form-actions">
            <button
              type="button"
              className="btn-primary"
              onClick={onClose}
              data-testid="team-invite-done"
            >
              Done
            </button>
          </div>
        </>
      )}
    </Modal>
  )
}
