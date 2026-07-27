import { useEffect, useMemo, useState } from 'react'
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
  const projectsLoaded = useProjectsStore((s) => s.projectsLoaded)
  const activeProjectId = useProjectsStore((s) => s.activeProjectId)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const setActiveProject = useProjectsStore((s) => s.setActiveProject)
  const deleteProject = useProjectsStore((s) => s.deleteProject)
  const updateProject = useProjectsStore((s) => s.updateProject)
  const cards = useProjectsStore((s) => s.cards)
  const fetchCards = useProjectsStore((s) => s.fetchCards)
  const projectsError = useProjectsStore((s) => s.projectsError)

  const [confirmDelete, setConfirmDelete] = useState<string | null>(null)
  const [editingProject, setEditingProject] = useState<string | null>(null)
  const [filter, setFilter] = useState('')

  const visibleProjects = useMemo(() => {
    const query = filter.trim().toLowerCase()
    if (query === '') return projects
    return projects.filter((p) => p.name.toLowerCase().includes(query))
  }, [projects, filter])

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
        extras={
          projects.length > 0 ? (
            <input
              className="list-view-search"
              type="search"
              placeholder="Filter projects…"
              aria-label="Filter projects by name"
              data-testid="project-filter"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
          ) : undefined
        }
      />
      {projectsError && projects.length > 0 && (
        <div className="fetch-error-banner" role="alert" data-testid="projects-error">
          <span>{projectsError}</span>
          <button type="button" onClick={() => fetchProjects()}>
            Retry
          </button>
        </div>
      )}
      {!projectsLoaded && !projectsError ? (
        // An unfinished first fetch is not “no projects” — hold the empty
        // state back until the list is actually known.
        <div className="list-view-body">
          <div className="list-view-empty" data-testid="projects-loading">
            Loading…
          </div>
        </div>
      ) : (
        <List<Project>
          items={visibleProjects}
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
            // Only a request that actually succeeded may claim “no projects”.
            projectsError ? (
              <div className="list-view-empty" role="alert" data-testid="projects-error">
                <p>{projectsError}</p>
                <button className="list-view-empty-action" onClick={() => fetchProjects()}>
                  Retry
                </button>
              </div>
            ) : filter.trim() ? (
              <div className="list-view-empty" data-testid="projects-filter-empty">
                <p>No projects match “{filter.trim()}”</p>
                <button className="list-view-empty-action" onClick={() => setFilter('')}>
                  Clear filter
                </button>
              </div>
            ) : (
              <div className="list-view-empty">
                <p>No projects yet</p>
                {onNewProject && (
                  <button className="list-view-empty-action" onClick={onNewProject}>
                    Create your first project
                  </button>
                )}
              </div>
            )
          }
        />
      )}

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
