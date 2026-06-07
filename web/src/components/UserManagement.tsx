import { useEffect, useState } from 'react'
import { useAuthStore } from '../store/auth'
import { useUsersStore } from '../store/users'

export default function UserManagement() {
  const currentUser = useAuthStore((s) => s.user)
  const users = useUsersStore((s) => s.users)
  const loading = useUsersStore((s) => s.loading)
  const storeError = useUsersStore((s) => s.error)
  const fetchUsers = useUsersStore((s) => s.fetchUsers)
  const createUserAction = useUsersStore((s) => s.createUser)
  const deleteUserAction = useUsersStore((s) => s.deleteUser)

  const [localError, setLocalError] = useState('')
  const error = localError || storeError

  // Create form
  const [showCreate, setShowCreate] = useState(false)
  const [newUsername, setNewUsername] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [newEmail, setNewEmail] = useState('')
  const [newRole, setNewRole] = useState('user')
  const [creating, setCreating] = useState(false)

  // Delete confirm
  const [deleteTarget, setDeleteTarget] = useState<string | null>(null)

  useEffect(() => {
    fetchUsers()
  }, [fetchUsers])

  if (currentUser?.role !== 'admin') {
    return (
      <div className="settings-page">
        <h2>User Management</h2>
        <p className="form-error">You do not have permission to access this page.</p>
      </div>
    )
  }

  const handleCreate = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!newUsername.trim() || !newPassword.trim()) return
    setCreating(true)
    setLocalError('')
    try {
      await createUserAction({
        username: newUsername.trim(),
        password: newPassword,
        role: newRole,
        email: newEmail.trim() || undefined,
      })
      setNewUsername('')
      setNewPassword('')
      setNewEmail('')
      setNewRole('user')
      setShowCreate(false)
    } catch (err) {
      setLocalError(err instanceof Error ? err.message : 'Failed to create user')
    } finally {
      setCreating(false)
    }
  }

  const handleDelete = async (id: string) => {
    setLocalError('')
    try {
      await deleteUserAction(id)
      setDeleteTarget(null)
    } catch (err) {
      setLocalError(err instanceof Error ? err.message : 'Failed to delete user')
    }
  }

  const formatDate = (dateStr: string): string => {
    try {
      return new Date(dateStr).toLocaleDateString(undefined, {
        year: 'numeric',
        month: 'short',
        day: 'numeric',
      })
    } catch {
      return dateStr
    }
  }

  const roleBadgeClass = (role: string): string => {
    switch (role) {
      case 'admin':
        return 'priority-high'
      case 'user':
        return 'priority-low'
      default:
        return 'priority-medium'
    }
  }

  return (
    <div className="settings-page">
      <h2>User Management</h2>

      {error && (
        <p className="form-error" style={{ marginBottom: 16 }}>
          {error}
        </p>
      )}

      <section className="settings-section">
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            marginBottom: 16,
          }}
        >
          <h3 style={{ margin: 0, border: 'none', paddingBottom: 0 }}>Users</h3>
          <button className="create-btn" onClick={() => setShowCreate(!showCreate)}>
            {showCreate ? 'Cancel' : 'Create User'}
          </button>
        </div>

        {showCreate && (
          <form onSubmit={handleCreate} className="form-inline-card" style={{ marginBottom: 16 }}>
            <div className="form-field">
              <label className="form-label">Username</label>
              <input
                className="form-input"
                value={newUsername}
                onChange={(e) => setNewUsername(e.target.value)}
                placeholder="username"
                required
              />
            </div>
            <div className="form-field">
              <label className="form-label">Password</label>
              <input
                className="form-input"
                type="password"
                value={newPassword}
                onChange={(e) => setNewPassword(e.target.value)}
                placeholder="password"
                required
              />
            </div>
            <div className="form-field">
              <label className="form-label">
                Email <span className="optional">(optional)</span>
              </label>
              <input
                className="form-input"
                type="email"
                value={newEmail}
                onChange={(e) => setNewEmail(e.target.value)}
                placeholder="user@example.com"
              />
            </div>
            <div className="form-field">
              <label className="form-label">Role</label>
              <select
                className="form-input"
                value={newRole}
                onChange={(e) => setNewRole(e.target.value)}
              >
                <option value="user">User</option>
                <option value="admin">Admin</option>
              </select>
            </div>
            <div className="form-actions">
              <button className="btn-secondary" type="button" onClick={() => setShowCreate(false)}>
                Cancel
              </button>
              <button
                className="btn-primary"
                type="submit"
                disabled={creating || !newUsername.trim() || !newPassword.trim()}
              >
                {creating ? 'Creating...' : 'Create'}
              </button>
            </div>
          </form>
        )}

        {loading && (
          <div className="chat-loading">
            <div className="loading-spinner" />
          </div>
        )}

        {!loading && users.length === 0 && (
          <p style={{ color: 'var(--text3)', fontSize: 'var(--text-sm)' }}>No users found.</p>
        )}

        {!loading && (
          <div className="folder-list">
            {users.map((u) => (
              <div key={u.id} className="folder-row">
                <div className="folder-info">
                  <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                    <strong>{u.username}</strong>
                    <span className={`priority-badge ${roleBadgeClass(u.role)}`}>{u.role}</span>
                  </div>
                  <span className="folder-path">
                    {u.email ? `${u.email} \u00B7 ` : ''}Created {formatDate(u.created_at)}
                  </span>
                </div>
                {u.id !== currentUser?.id && (
                  <>
                    {deleteTarget === u.id ? (
                      <div style={{ display: 'flex', gap: 4 }}>
                        <button
                          className="btn-primary"
                          style={{
                            fontSize: 'var(--text-xs)',
                            padding: '4px 10px',
                            background: 'var(--danger)',
                          }}
                          onClick={() => handleDelete(u.id)}
                        >
                          Confirm
                        </button>
                        <button
                          className="btn-secondary"
                          style={{ fontSize: 'var(--text-xs)', padding: '4px 10px' }}
                          onClick={() => setDeleteTarget(null)}
                        >
                          Cancel
                        </button>
                      </div>
                    ) : (
                      <button
                        className="folder-delete"
                        onClick={() => setDeleteTarget(u.id)}
                        title="Delete user"
                      >
                        &times;
                      </button>
                    )}
                  </>
                )}
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  )
}
