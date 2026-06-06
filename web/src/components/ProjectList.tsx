import { useEffect, useState } from 'react'
import { useProjectsStore } from '../store/projects'

export default function ProjectList() {
  const projects = useProjectsStore((s) => s.projects)
  const activeProjectId = useProjectsStore((s) => s.activeProjectId)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const setActiveProject = useProjectsStore((s) => s.setActiveProject)
  const createProject = useProjectsStore((s) => s.createProject)
  const cards = useProjectsStore((s) => s.cards)
  const fetchCards = useProjectsStore((s) => s.fetchCards)

  const [showCreate, setShowCreate] = useState(false)
  const [newName, setNewName] = useState('')
  const [creating, setCreating] = useState(false)
  const [error, setError] = useState('')

  useEffect(() => {
    fetchProjects()
  }, [fetchProjects])

  useEffect(() => {
    if (activeProjectId) {
      fetchCards(activeProjectId)
    }
  }, [activeProjectId, fetchCards])

  const handleCreate = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!newName.trim()) return
    setCreating(true)
    setError('')
    try {
      await createProject({ name: newName.trim() })
      setNewName('')
      setShowCreate(false)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Failed to create project')
    } finally {
      setCreating(false)
    }
  }

  return (
    <div className="project-list-container">
      <div className="project-list-header">
        <h2>Projects</h2>
        <button className="create-btn" onClick={() => setShowCreate(!showCreate)}>
          {showCreate ? 'Cancel' : 'New Project'}
        </button>
      </div>

      {showCreate && (
        <form className="create-form" onSubmit={handleCreate}>
          <input
            type="text"
            placeholder="Project name"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            autoFocus
          />
          <button type="submit" disabled={creating || !newName.trim()}>
            {creating ? 'Creating...' : 'Create'}
          </button>
          {error && <p className="form-error">{error}</p>}
        </form>
      )}

      <ul className="project-list">
        {projects.map((project) => {
          const cardCount =
            project.id === activeProjectId ? cards.length : null
          return (
            <li
              key={project.id}
              className={project.id === activeProjectId ? 'active' : ''}
            >
              <button onClick={() => setActiveProject(project.id)}>
                <span className="project-name">{project.name}</span>
                <span className={`status-badge status-${project.status}`}>
                  {project.status}
                </span>
                {cardCount !== null && (
                  <span className="card-count">{cardCount} cards</span>
                )}
              </button>
            </li>
          )
        })}
        {projects.length === 0 && <li className="empty">No projects yet</li>}
      </ul>
    </div>
  )
}
