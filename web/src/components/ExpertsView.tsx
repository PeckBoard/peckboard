import { useEffect, useMemo } from 'react'
import { useSessionsStore } from '../store/sessions'
import { useProjectsStore } from '../store/projects'
import type { Expert } from '../types/api'

/**
 * Expert Sessions view.
 *
 * Experts are long-lived agent sessions that hold codebase knowledge.
 * They are hidden from the ordinary chat list, so this is the only place
 * a user can see them. Experts are organised into sections by scope:
 *
 *   - "Global" — experts with no project (`project_id == null`). These
 *     serve chat sessions across the whole install (e.g. the default
 *     question-expert).
 *   - One section per project — experts scoped to that project.
 *
 * The schema links an expert to a project (or global), not to an
 * individual chat session, so "by chat session" maps to the Global scope
 * here: the experts every chat session can consult. Within each section,
 * a badge distinguishes question-experts from knowledge-experts.
 */

const GLOBAL_KEY = '__global__'

function kindLabel(kind: string | null): string {
  if (kind === 'question') return 'Question'
  if (kind === 'knowledge') return 'Knowledge'
  return 'Expert'
}

function ExpertRow({
  expert,
  projectLabel,
  onOpen,
}: {
  expert: Expert
  projectLabel: string
  onOpen: (id: string) => void
}) {
  const kind = expert.expert_kind
  return (
    <div
      className="expert-row expert-row-clickable"
      data-testid="expert-row"
      data-expert-kind={kind ?? 'expert'}
      role="button"
      tabIndex={0}
      onClick={() => onOpen(expert.id)}
      onKeyDown={(e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          onOpen(expert.id)
        }
      }}
    >
      <div className="expert-row-head">
        <span className="expert-name">{expert.name}</span>
        <span className={`expert-kind-badge expert-kind-${kind ?? 'expert'}`}>
          {kindLabel(kind)}
        </span>
      </div>
      <div className="expert-meta">
        {expert.knowledge_area && (
          <span className="expert-area" title="Knowledge area">
            {expert.knowledge_area}
          </span>
        )}
        <span className="expert-project" title="Project">
          {projectLabel}
        </span>
      </div>
      {expert.knowledge_summary && <p className="expert-summary">{expert.knowledge_summary}</p>}
      <div className="expert-boundaries">
        <span className="expert-boundaries-label">Boundaries:</span>{' '}
        <span className="expert-scope">{expert.scope_path || 'whole project'}</span>
      </div>
    </div>
  )
}

export default function ExpertsView({ onOpenExpert }: { onOpenExpert: (id: string) => void }) {
  const experts = useSessionsStore((s) => s.experts)
  const expertsLoaded = useSessionsStore((s) => s.expertsLoaded)
  const fetchExperts = useSessionsStore((s) => s.fetchExperts)
  const projects = useProjectsStore((s) => s.projects)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)

  useEffect(() => {
    fetchExperts()
    fetchProjects()
  }, [fetchExperts, fetchProjects])

  const projectName = useMemo(() => {
    const m = new Map<string, string>()
    for (const p of projects) m.set(p.id, p.name)
    return m
  }, [projects])

  // Group experts by project_id (null -> Global), preserving the
  // last_activity ordering the API already applied within each group.
  const groups = useMemo(() => {
    const byKey = new Map<string, Expert[]>()
    for (const e of experts) {
      const key = e.project_id ?? GLOBAL_KEY
      const list = byKey.get(key) ?? []
      list.push(e)
      byKey.set(key, list)
    }
    // Global first, then projects sorted by name.
    const ordered: { key: string; title: string; experts: Expert[] }[] = []
    if (byKey.has(GLOBAL_KEY)) {
      ordered.push({
        key: GLOBAL_KEY,
        title: 'Global · Chat Sessions',
        experts: byKey.get(GLOBAL_KEY)!,
      })
    }
    const projectKeys = [...byKey.keys()].filter((k) => k !== GLOBAL_KEY)
    projectKeys.sort((a, b) => (projectName.get(a) ?? a).localeCompare(projectName.get(b) ?? b))
    for (const k of projectKeys) {
      ordered.push({
        key: k,
        title: projectName.get(k) ?? 'Unknown Project',
        experts: byKey.get(k)!,
      })
    }
    return ordered
  }, [experts, projectName])

  return (
    <div className="list-view experts-view">
      <div className="list-view-header">
        <h2 className="list-view-title">Experts</h2>
      </div>
      <div className="list-view-body">
        {expertsLoaded && experts.length === 0 && (
          <div className="list-view-empty">
            <p>No expert sessions yet</p>
            <p className="experts-empty-hint">
              Experts are spun up per project to hold codebase knowledge and answer questions from
              chat and worker sessions.
            </p>
          </div>
        )}
        {groups.map((group) => (
          <section key={group.key} className="expert-group" data-testid="expert-group">
            <h3 className="expert-group-title">
              {group.title}
              <span className="expert-group-count">{group.experts.length}</span>
            </h3>
            <div className="expert-group-body">
              {group.experts.map((expert) => (
                <ExpertRow
                  key={expert.id}
                  expert={expert}
                  onOpen={onOpenExpert}
                  projectLabel={
                    expert.project_id ? (projectName.get(expert.project_id) ?? 'Project') : 'Global'
                  }
                />
              ))}
            </div>
          </section>
        ))}
      </div>
    </div>
  )
}
