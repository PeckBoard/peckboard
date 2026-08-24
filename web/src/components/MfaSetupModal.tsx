import { useState, type FormEvent } from 'react'
import { authedFetch } from '../store/auth'
import Modal from './Modal'
import FieldError from './FieldError'

type Step = 'password' | 'confirm' | 'codes'

interface Props {
  onClose: () => void
  onEnabled: () => void
}

export default function MfaSetupModal({ onClose, onEnabled }: Props) {
  const [step, setStep] = useState<Step>('password')
  const [password, setPassword] = useState('')
  const [code, setCode] = useState('')
  const [secret, setSecret] = useState('')
  const [qrSvg, setQrSvg] = useState('')
  const [otpauth, setOtpauth] = useState('')
  const [recovery, setRecovery] = useState<string[]>([])
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)

  const begin = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    setLoading(true)
    try {
      const res = await authedFetch('/api/auth/mfa/totp/begin', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password }),
      })
      const data = await res.json().catch(() => ({ error: 'Failed to start setup' }))
      if (!res.ok) throw new Error(data.error || 'Failed to start setup')
      setSecret(data.secret as string)
      setQrSvg((data.qr_svg as string) || '')
      setOtpauth((data.otpauth_url as string) || '')
      setStep('confirm')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to start setup')
    } finally {
      setLoading(false)
    }
  }

  const confirm = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    setLoading(true)
    try {
      const res = await authedFetch('/api/auth/mfa/totp/confirm', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password, code }),
      })
      const data = await res.json().catch(() => ({ error: 'Failed to confirm' }))
      if (!res.ok) throw new Error(data.error || 'Failed to confirm')
      setRecovery((data.recovery_codes as string[]) || [])
      setStep('codes')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to confirm')
    } finally {
      setLoading(false)
    }
  }

  const downloadCodes = () => {
    const blob = new Blob([recovery.join('\n') + '\n'], { type: 'text/plain' })
    const url = URL.createObjectURL(blob)
    const a = document.createElement('a')
    a.href = url
    a.download = 'peckboard-recovery-codes.txt'
    a.click()
    URL.revokeObjectURL(url)
  }

  return (
    <Modal onClose={step === 'codes' ? undefined : onClose} data-testid="mfa-setup-modal">
      {step === 'password' && (
        <>
          <h2>Enable Two-Factor Auth</h2>
          <p className="form-hint">
            Confirm your password, then scan the QR code with an authenticator app.
          </p>
          <form onSubmit={begin}>
            <div className="form-field">
              <label className="form-label" htmlFor="mfa-setup-password">
                Current password
              </label>
              <input
                id="mfa-setup-password"
                className="form-input"
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                autoFocus
                required
                data-testid="mfa-setup-password"
              />
            </div>
            <FieldError message={error} testId="mfa-setup-error" />
            <div className="form-actions">
              <button type="button" className="btn-secondary" onClick={onClose}>
                Cancel
              </button>
              <button className="btn-primary" type="submit" disabled={loading || !password}>
                {loading ? 'Starting…' : 'Continue'}
              </button>
            </div>
          </form>
        </>
      )}

      {step === 'confirm' && (
        <>
          <h2>Scan the QR Code</h2>
          <p className="form-hint">
            Add Peckboard in your authenticator app, then enter the 6-digit code.
          </p>
          {qrSvg ? (
            <div
              className="mfa-qr"
              data-testid="mfa-qr"
              dangerouslySetInnerHTML={{ __html: qrSvg }}
            />
          ) : null}
          <div className="form-field">
            <label className="form-label" htmlFor="mfa-secret">
              Secret (manual entry)
            </label>
            <input
              id="mfa-secret"
              className="form-input"
              readOnly
              value={secret}
              data-testid="mfa-secret"
            />
            {otpauth ? (
              <span className="form-hint" data-testid="mfa-otpauth">
                {otpauth}
              </span>
            ) : null}
          </div>
          <form onSubmit={confirm}>
            <div className="form-field">
              <label className="form-label" htmlFor="mfa-confirm-code">
                Authenticator code
              </label>
              <input
                id="mfa-confirm-code"
                className="form-input"
                inputMode="numeric"
                autoComplete="one-time-code"
                value={code}
                onChange={(e) => setCode(e.target.value)}
                required
                data-testid="mfa-confirm-code"
              />
            </div>
            <FieldError message={error} testId="mfa-setup-error" />
            <div className="form-actions">
              <button type="button" className="btn-secondary" onClick={onClose}>
                Cancel
              </button>
              <button className="btn-primary" type="submit" disabled={loading || code.length < 6}>
                {loading ? 'Confirming…' : 'Confirm'}
              </button>
            </div>
          </form>
        </>
      )}

      {step === 'codes' && (
        <>
          <h2>Save Recovery Codes</h2>
          <p className="form-hint">
            Each code signs you in once if you lose the authenticator. Store them offline — they
            will not be shown again.
          </p>
          <ul className="mfa-recovery-list" data-testid="mfa-recovery-codes">
            {recovery.map((c) => (
              <li key={c}>
                <code>{c}</code>
              </li>
            ))}
          </ul>
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={downloadCodes}>
              Download
            </button>
            <button
              type="button"
              className="btn-primary"
              data-testid="mfa-setup-done"
              onClick={() => {
                onEnabled()
                onClose()
              }}
            >
              Done
            </button>
          </div>
        </>
      )}
    </Modal>
  )
}

interface ActionProps {
  title: string
  message: string
  confirmLabel: string
  danger?: boolean
  onClose: () => void
  onSubmit: (password: string, method: string, code: string) => Promise<string[] | void>
}

/** Disable or regenerate-recovery: password + TOTP/recovery code. */
export function MfaActionModal({
  title,
  message,
  confirmLabel,
  danger,
  onClose,
  onSubmit,
}: ActionProps) {
  const [password, setPassword] = useState('')
  const [code, setCode] = useState('')
  const [useRecovery, setUseRecovery] = useState(false)
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
  const [codes, setCodes] = useState<string[] | null>(null)

  const handle = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    setLoading(true)
    try {
      const result = await onSubmit(password, useRecovery ? 'recovery' : 'totp', code)
      if (Array.isArray(result)) setCodes(result)
      else onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed')
    } finally {
      setLoading(false)
    }
  }

  if (codes) {
    return (
      <Modal onClose={onClose} data-testid="mfa-action-modal">
        <h2>New Recovery Codes</h2>
        <p className="form-hint">Save these now. Previous codes no longer work.</p>
        <ul className="mfa-recovery-list" data-testid="mfa-recovery-codes">
          {codes.map((c) => (
            <li key={c}>
              <code>{c}</code>
            </li>
          ))}
        </ul>
        <div className="form-actions">
          <button
            type="button"
            className="btn-primary"
            onClick={onClose}
            data-testid="mfa-action-done"
          >
            Done
          </button>
        </div>
      </Modal>
    )
  }

  return (
    <Modal onClose={onClose} data-testid="mfa-action-modal">
      <h2>{title}</h2>
      <p className="form-hint">{message}</p>
      <form onSubmit={handle}>
        <div className="form-field">
          <label className="form-label" htmlFor="mfa-action-password">
            Current password
          </label>
          <input
            id="mfa-action-password"
            className="form-input"
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoFocus
            required
            data-testid="mfa-action-password"
          />
        </div>
        <div className="form-field">
          <label className="form-label" htmlFor="mfa-action-code">
            {useRecovery ? 'Recovery code' : 'Authenticator code'}
          </label>
          <input
            id="mfa-action-code"
            className="form-input"
            type="text"
            value={code}
            onChange={(e) => setCode(e.target.value)}
            required
            data-testid="mfa-action-code"
          />
        </div>
        <button
          type="button"
          className="btn-link"
          onClick={() => setUseRecovery((v) => !v)}
          data-testid="mfa-action-recovery-toggle"
        >
          {useRecovery ? 'Use authenticator code' : 'Use a recovery code'}
        </button>
        <FieldError message={error} testId="mfa-action-error" />
        <div className="form-actions">
          <button type="button" className="btn-secondary" onClick={onClose}>
            Cancel
          </button>
          <button
            className={danger ? 'btn-danger' : 'btn-primary'}
            type="submit"
            disabled={loading || !password || !code}
            data-testid="mfa-action-confirm"
          >
            {loading ? 'Working…' : confirmLabel}
          </button>
        </div>
      </form>
    </Modal>
  )
}
