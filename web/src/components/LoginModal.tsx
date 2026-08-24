import { useState, type FormEvent } from 'react'
import { useAuthStore } from '../store/auth'
import Modal from './Modal'
import FieldError from './FieldError'

export default function LoginModal() {
  const login = useAuthStore((s) => s.login)
  const verifyMfa = useAuthStore((s) => s.verifyMfa)
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [rememberMe, setRememberMe] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [challenge, setChallenge] = useState<string | null>(null)
  const [methods, setMethods] = useState<string[]>([])
  const [useRecovery, setUseRecovery] = useState(false)
  const [code, setCode] = useState('')

  const handlePassword = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    setLoading(true)
    try {
      const result = await login(username, password, rememberMe)
      if (result.mfa) {
        setChallenge(result.mfa.challenge)
        setMethods(result.mfa.methods)
        setUseRecovery(result.mfa.methods.includes('totp') ? false : true)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Login failed')
    } finally {
      setLoading(false)
    }
  }

  const handleMfa = async (e: FormEvent) => {
    e.preventDefault()
    if (!challenge) return
    setError(null)
    setLoading(true)
    try {
      const method = useRecovery ? 'recovery' : 'totp'
      await verifyMfa(challenge, method, code, rememberMe)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Login failed')
    } finally {
      setLoading(false)
    }
  }

  const backToPassword = () => {
    setChallenge(null)
    setCode('')
    setError(null)
    setUseRecovery(false)
  }

  return (
    <Modal>
      <img src="/favicon.svg" alt="" width="64" height="64" className="modal-brand-icon" />
      <h1 className="modal-brand">
        Peck<span>board</span>
      </h1>
      <p className="modal-subtitle">
        {challenge ? 'Enter your authentication code' : 'Sign in to your account'}
      </p>
      {challenge ? (
        <form onSubmit={handleMfa}>
          <div className="form-field">
            <label className="form-label" htmlFor="login-mfa-code">
              {useRecovery ? 'Recovery code' : 'Authenticator code'}
            </label>
            <input
              id="login-mfa-code"
              className="form-input"
              type="text"
              inputMode={useRecovery ? 'text' : 'numeric'}
              autoComplete={useRecovery ? 'off' : 'one-time-code'}
              value={code}
              onChange={(e) => setCode(e.target.value)}
              autoFocus
              required
              data-testid="login-mfa-code"
            />
          </div>
          {methods.includes('recovery') && methods.includes('totp') && (
            <button
              type="button"
              className="btn-link"
              data-testid="login-mfa-recovery-toggle"
              onClick={() => {
                setUseRecovery((v) => !v)
                setCode('')
                setError(null)
              }}
            >
              {useRecovery ? 'Use authenticator code' : 'Use a recovery code'}
            </button>
          )}
          <FieldError message={error ?? undefined} testId="login-mfa-error" />
          <button
            className="btn-primary"
            type="submit"
            disabled={loading}
            data-testid="login-mfa-submit"
          >
            {loading ? 'Verifying…' : 'Verify'}
          </button>
          <button type="button" className="btn-secondary" onClick={backToPassword}>
            Back
          </button>
        </form>
      ) : (
        <form onSubmit={handlePassword}>
          <div className="form-field">
            <label className="form-label" htmlFor="login-username">
              Username
            </label>
            <input
              id="login-username"
              className="form-input"
              type="text"
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              autoFocus
              required
            />
          </div>
          <div className="form-field">
            <label className="form-label" htmlFor="login-password">
              Password
            </label>
            <input
              id="login-password"
              className="form-input"
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              required
            />
          </div>
          <div
            className="form-field"
            style={{ flexDirection: 'row', alignItems: 'center', gap: '0.5rem' }}
          >
            <input
              id="login-remember"
              type="checkbox"
              checked={rememberMe}
              onChange={(e) => setRememberMe(e.target.checked)}
            />
            <label className="form-label" htmlFor="login-remember" style={{ margin: 0 }}>
              Remember me
            </label>
          </div>
          {error && <p className="form-error">{error}</p>}
          <button className="btn-primary" type="submit" disabled={loading}>
            {loading ? 'Signing in...' : 'Sign In'}
          </button>
        </form>
      )}
    </Modal>
  )
}
