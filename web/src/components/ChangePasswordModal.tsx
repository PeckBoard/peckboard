import { useState, type FormEvent } from 'react'
import { useAuthStore } from '../store/auth'
import { useUsersStore } from '../store/users'

const MIN_PASSWORD_LEN = 12

type Mode = { kind: 'self' } | { kind: 'admin'; targetUserId: string; targetUsername: string }

interface Props {
  mode: Mode
  onClose: () => void
}

/**
 * Single modal that handles both flows:
 * - `self`: the signed-in user changes their own password. Requires the
 *   current password; on success the server hands back a fresh token and
 *   the auth store keeps the session alive.
 * - `admin`: an admin resets another user's password. No current password
 *   required; the target's existing auth sessions are revoked so any live
 *   tokens belonging to them stop working immediately.
 */
export default function ChangePasswordModal({ mode, onClose }: Props) {
  const changePassword = useAuthStore((s) => s.changePassword)
  const setUserPassword = useUsersStore((s) => s.setUserPassword)

  const [currentPassword, setCurrentPassword] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const [error, setError] = useState('')
  const [loading, setLoading] = useState(false)
  const [done, setDone] = useState(false)

  const title =
    mode.kind === 'self' ? 'Change Password' : `Reset password for ${mode.targetUsername}`

  const handleSubmit = async (e: FormEvent) => {
    e.preventDefault()
    setError('')
    if (newPassword.length < MIN_PASSWORD_LEN) {
      setError(`New password must be at least ${MIN_PASSWORD_LEN} characters`)
      return
    }
    if (newPassword !== confirmPassword) {
      setError("Passwords don't match")
      return
    }
    setLoading(true)
    try {
      if (mode.kind === 'self') {
        await changePassword(currentPassword, newPassword)
        onClose()
      } else {
        await setUserPassword(mode.targetUserId, newPassword)
        setDone(true)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to change password')
    } finally {
      setLoading(false)
    }
  }

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>{title}</h2>
        {done ? (
          <>
            <p style={{ color: 'var(--text2)', fontSize: 'var(--text-sm)' }}>
              Password updated. Any existing sessions for{' '}
              {mode.kind === 'admin' ? mode.targetUsername : 'this user'} have been revoked.
            </p>
            <div className="form-actions">
              <button type="button" className="btn-primary" onClick={onClose}>
                Done
              </button>
            </div>
          </>
        ) : (
          <form onSubmit={handleSubmit}>
            {mode.kind === 'self' && (
              <div className="form-field">
                <label className="form-label" htmlFor="cp-current">
                  Current password
                </label>
                <input
                  id="cp-current"
                  className="form-input"
                  type="password"
                  value={currentPassword}
                  onChange={(e) => setCurrentPassword(e.target.value)}
                  autoFocus
                  required
                />
              </div>
            )}
            <div className="form-field">
              <label className="form-label" htmlFor="cp-new">
                New password
              </label>
              <input
                id="cp-new"
                className="form-input"
                type="password"
                value={newPassword}
                onChange={(e) => setNewPassword(e.target.value)}
                autoFocus={mode.kind === 'admin'}
                minLength={MIN_PASSWORD_LEN}
                required
              />
              <span className="form-hint">At least {MIN_PASSWORD_LEN} characters</span>
            </div>
            <div className="form-field">
              <label className="form-label" htmlFor="cp-confirm">
                Confirm new password
              </label>
              <input
                id="cp-confirm"
                className="form-input"
                type="password"
                value={confirmPassword}
                onChange={(e) => setConfirmPassword(e.target.value)}
                required
              />
            </div>
            {error && <p className="form-error">{error}</p>}
            <div className="form-actions">
              <button type="button" className="btn-secondary" onClick={onClose}>
                Cancel
              </button>
              <button
                className="btn-primary"
                type="submit"
                disabled={
                  loading ||
                  newPassword.length < MIN_PASSWORD_LEN ||
                  newPassword !== confirmPassword ||
                  (mode.kind === 'self' && !currentPassword)
                }
              >
                {loading
                  ? 'Saving...'
                  : mode.kind === 'self'
                    ? 'Change Password'
                    : 'Reset Password'}
              </button>
            </div>
          </form>
        )}
      </div>
    </div>
  )
}
