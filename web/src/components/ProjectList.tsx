import { useEffect, useState } from 'react'
import { useProjectsStore } from '../store/projects'
import { useTabsStore } from '../store/tabs'
import ConfirmDialog from './ConfirmDialog'
import EditProjectModal from './EditProjectModal'
import List from './List'
import ListViewHeader from './ListViewHeader'
import type { MenuItem } from './Dropdown'
import type { Project } from '../types/api'

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
  const [editingProject, setEditingProject] = useState<string | null>(null)

  useEffect(() => {
    fetchProjects()
  }, [fetchProjects])

  useEffect(() => {
    if (activeProjectId) {
      fetchCards(activeProjectId)
    }
  }, [activeProjectId, fetchCards])

  const handleTogglePause = async (project: Project) => {
    await updateProject(project.id, {
      status: project.status === 'paused' ? 'active' : 'paused',
    })
  }

  const handleDelete = async (projectId: string) => {
    setConfirmDelete(null)
    await deleteProject(projectId)
  }

  const buildMenu = (project: Project): MenuItem[] => [
    { label: 'Edit', onSelect: () => setEditingProject(project.id) },
    { divider: true },
    {
      label: project.status === 'paused' ? 'Resume' : 'Pause',
      onSelect: () => handleTogglePause(project),
    },
    { divider: true },
    {
      label: 'Delete',
      danger: true,
      onSelect: () => setConfirmDelete(project.id),
    },
  ]

  return (
    <>
      <ListViewHeader
        title="Projects"
        actionLabel={onNewProject ? '+ New project' : undefined}
        onAction={onNewProject}
      />
      <List<Project>
        items={projects}
        getKey={(p) => p.id}
        activeId={activeProjectId}
        onActivate={(p) => {
          setActiveProject(p.id)
          useTabsStore.getState().openTab('project', p.id)
        }}
        getMenuItems={buildMenu}
        renderItem={(project) => (
          <>
            {project.status !== 'active' && (
              <span className={`status-badge status-${project.status}`}>{project.status}</span>
            )}
            <span className="list-view-name">{project.name}</span>
            <span className="list-view-meta">
              {project.id === activeProjectId && (
                <span className="list-view-tag">{cards.length} cards</span>
              )}
            </span>
          </>
        )}
        emptyState={
          <div className="list-view-empty">
            <p>No projects yet</p>
            {onNewProject && (
              <button className="list-view-empty-action" onClick={onNewProject}>
                Create your first project
              </button>
            )}
          </div>
        }
      />

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
