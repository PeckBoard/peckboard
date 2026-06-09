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
    <>
      <div className="list-view-header">
        <h2 className="list-view-title">Projects</h2>
        {onNewProject && (
          <button className="list-view-action" onClick={onNewProject}>
            + New project
          </button>
        )}
      </div>
      <div className="list-view-body">
        {projects.length === 0 ? (
          <div className="list-view-empty">
            <p>No projects yet</p>
            {onNewProject && (
              <button className="list-view-empty-action" onClick={onNewProject}>
                Create your first project
              </button>
            )}
          </div>
        ) : (
          projects.map((project) => {
            const cardCount = project.id === activeProjectId ? cards.length : null
            return (
              <div
                key={project.id}
                className={`list-view-row ${project.id === activeProjectId ? 'active' : ''}`}
              >
                <button className="list-view-item" onClick={() => setActiveProject(project.id)}>
                  {project.status !== 'active' && (
                    <span className={`status-badge status-${project.status}`}>
                      {project.status}
                    </span>
                  )}
                  <span className="list-view-name">{project.name}</span>
                  <span className="list-view-meta">
                    {cardCount !== null && <span className="list-view-tag">{cardCount} cards</span>}
                  </span>
                </button>
                <button
                  className="list-view-menu"
                  onClick={(e) => {
                    e.stopPropagation()
                    setMenuOpen(menuOpen === project.id ? null : project.id)
                  }}
                  aria-label="Project menu"
                >
                  ···
                </button>
                {menuOpen === project.id && (
                  <div className="list-view-dropdown">
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
            )
          })
        )}
      </div>

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
    </>
  )
}
