import { useEffect, useState } from 'react'
import { useProjectsStore } from '../store/projects'
import ConfirmDialog from './ConfirmDialog'
import EditProjectModal from './EditProjectModal'

interface ProjectListProps {
  onNewProject?: () => void
}

export default function ProjectList({ onNewProject }: ProjectListProps) {
  const projects = useProjectsStore((s) => s.projects)
  const activeProjectId = useProjectsStore((s) => s.activeProjectId)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const setActiveProject = useProjectsStore((s) => s.setActiveProject)
  const deleteProject = useProjectsStore((s) => s.deleteProject)
  const updateProject = useProjectsStore((s) => s.updateProject)
  const cards = useProjectsStore((s) => s.cards)
  const fetchCards = useProjectsStore((s) => s.fetchCards)

  const [confirmDelete, setConfirmDelete] = useState<string | null>(null)
  const [menuOpen, setMenuOpen] = useState<string | null>(null)
  const [editingProject, setEditingProject] = useState<string | null>(null)

  useEffect(() => {
    fetchProjects()
  }, [fetchProjects])

  useEffect(() => {
    if (activeProjectId) {
      fetchCards(activeProjectId)
    }
  }, [activeProjectId, fetchCards])

  const handleTogglePause = async (projectId: string, currentStatus: string) => {
    setMenuOpen(null)
    await updateProject(projectId, {
      status: currentStatus === 'paused' ? 'active' : 'paused',
    })
  }

  const handleDelete = async (projectId: string) => {
    setConfirmDelete(null)
    setMenuOpen(null)
    await deleteProject(projectId)
  }

  return (
    <div className="project-list-container">
      <div className="project-list-header">
        <h2>Projects</h2>
        {onNewProject && (
          <button className="create-btn" onClick={onNewProject}>
            + New
          </button>
        )}
      </div>

      <ul className="project-list">
        {projects.map((project) => {
          const cardCount = project.id === activeProjectId ? cards.length : null
          return (
            <li key={project.id} className={project.id === activeProjectId ? 'active' : ''}>
              <button className="project-list-item" onClick={() => setActiveProject(project.id)}>
                <span className="project-name">{project.name}</span>
                <span className={`status-badge status-${project.status}`}>{project.status}</span>
                {cardCount !== null && <span className="card-count">{cardCount} cards</span>}
              </button>
              <div className="project-list-actions">
                <button
                  className="project-menu-btn"
                  onClick={(e) => {
                    e.stopPropagation()
                    setMenuOpen(menuOpen === project.id ? null : project.id)
                  }}
                >
                  <svg width="14" height="14" viewBox="0 0 16 16" fill="currentColor">
                    <circle cx="8" cy="3" r="1.5" />
                    <circle cx="8" cy="8" r="1.5" />
                    <circle cx="8" cy="13" r="1.5" />
                  </svg>
                </button>
                {menuOpen === project.id && (
                  <div className="project-menu-dropdown">
                    <button
                      onClick={() => {
                        setMenuOpen(null)
                        setEditingProject(project.id)
                      }}
                    >
                      Edit
                    </button>
                    <button onClick={() => handleTogglePause(project.id, project.status)}>
                      {project.status === 'paused' ? 'Resume' : 'Pause'}
                    </button>
                    <button
                      className="danger"
                      onClick={() => {
                        setMenuOpen(null)
                        setConfirmDelete(project.id)
                      }}
                    >
                      Delete
                    </button>
                  </div>
                )}
              </div>
            </li>
          )
        })}
        {projects.length === 0 && <li className="empty">No projects yet</li>}
      </ul>

      {editingProject &&
        (() => {
          const proj = projects.find((p) => p.id === editingProject)
          return proj ? (
            <EditProjectModal project={proj} onClose={() => setEditingProject(null)} />
          ) : null
        })()}

      {confirmDelete && (
        <ConfirmDialog
          title="Delete project"
          message="Delete this project, all its cards, and worker sessions? This cannot be undone."
          confirmLabel="Delete"
          cancelLabel="Cancel"
          danger
          onConfirm={() => handleDelete(confirmDelete)}
          onCancel={() => setConfirmDelete(null)}
        />
      )}
    </div>
  )
}
