import { useState, type FormEvent } from 'react'
import { useAuthStore } from '../store/auth'
import Modal from './Modal'

export default function LoginModal() {
  const login = useAuthStore((s) => s.login)
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [rememberMe, setRememberMe] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    setError(null)
    setLoading(true)
    try {
      await login(username, password, rememberMe)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Login failed')
    } finally {
      setLoading(false)
    }
  }

  return (
    <Modal>
      <img src="/favicon.svg" alt="" width="64" height="64" className="modal-brand-icon" />
      <h1 className="modal-brand">
        Peck<span>board</span>
      </h1>
      <p className="modal-subtitle">Sign in to your account</p>
      <form onSubmit={handleSubmit}>
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
    </Modal>
  )
}
