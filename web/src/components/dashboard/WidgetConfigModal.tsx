import { useEffect, useState } from 'react'
import { authedFetch } from '../../store/auth'
import { useProjectsStore } from '../../store/projects'
import { useReportsStore } from '../../store/reports'
import type { ViewWidget, WidgetField } from '../../store/views'
import Modal from '../Modal'
import { MenuButton, type MenuItem } from '../Dropdown'
import { WIDGET_SPECS } from './registry'
import { SshFleetError, sshFleetFetch, type SshHost } from './widgets/sshFleet'
import '../../styles/dashboard-info.css'

type Patch = Partial<Pick<ViewWidget, WidgetField>>

interface CardOption {
  id: string
  title: string
  step: string
}

/** Single-choice searchable combobox styled as a form field. */
function Picker({
  id,
  testId,
  items,
  value,
  placeholder,
  emptyLabel,
  onOpen,
}: {
  id: string
  testId: string
  items: MenuItem[]
  value: string
  placeholder: string
  emptyLabel: string
  onOpen?: () => void
}) {
  return (
    <MenuButton
      id={id}
      testId={testId}
      searchTestId={`${testId}-search`}
      items={items}
      searchable
      haspopup="listbox"
      matchTriggerWidth
      align="left"
      ariaLabel={placeholder}
      listLabel={placeholder}
      searchPlaceholder="Search…"
      emptyLabel={emptyLabel}
      triggerClassName="form-input dash-config-picker"
      onOpen={onOpen}
    >
      <span className="dash-config-picker-value">{value}</span>
      <span className="dash-config-picker-caret" aria-hidden="true">
        ▾
      </span>
    </MenuButton>
  )
}

/**
 * "Configure…" for an info widget: its project scope (with "All projects"
 * for optionally-scoped kinds), the dependencies root card, the pinned
 * report, or the SSH host. Every pick persists immediately via `onPatch`.
 */
export default function WidgetConfigModal({
  widget,
  cardTitle,
  onPatch,
  onClose,
}: {
  widget: ViewWidget
  /** Title of the current root card from the view's `cards` meta. */
  cardTitle?: string
  onPatch: (patch: Patch) => void
  onClose: () => void
}) {
  const spec = WIDGET_SPECS[widget.kind]
  const projects = useProjectsStore((s) => s.projects)
  const projectsLoaded = useProjectsStore((s) => s.projectsLoaded)
  const fetchProjects = useProjectsStore((s) => s.fetchProjects)
  const reports = useReportsStore((s) => s.reports)
  const reportsLoading = useReportsStore((s) => s.loading)
  const fetchReports = useReportsStore((s) => s.fetchReports)
  const projectId = widget.projectId ?? null
  const [cards, setCards] = useState<{ projectId: string; list: CardOption[] } | null>(null)
  const [hosts, setHosts] = useState<SshHost[] | null>(null)
  const [hostsError, setHostsError] = useState<string | null>(null)

  useEffect(() => {
    if (spec.scope !== 'none') void fetchProjects()
    if (widget.kind === 'report') void fetchReports()
  }, [spec.scope, widget.kind, fetchProjects, fetchReports])

  useEffect(() => {
    if (widget.kind !== 'dependencies' || !projectId) return
    let cancelled = false
    authedFetch(`/api/projects/${encodeURIComponent(projectId)}/cards`)
      .then((res) => (res.ok ? (res.json() as Promise<CardOption[]>) : []))
      .then((list) => {
        if (!cancelled) setCards({ projectId, list: Array.isArray(list) ? list : [] })
      })
      .catch(() => {
        if (!cancelled) setCards({ projectId, list: [] })
      })
    return () => {
      cancelled = true
    }
  }, [widget.kind, projectId])

  useEffect(() => {
    if (widget.kind !== 'ssh_activity') return
    let cancelled = false
    sshFleetFetch<{ hosts?: SshHost[] }>('/hosts')
      .then((d) => {
        if (!cancelled) setHosts(d.hosts ?? [])
      })
      .catch((e: unknown) => {
        if (cancelled) return
        setHosts([])
        setHostsError(
          e instanceof SshFleetError && e.notInstalled
            ? 'SSH Fleet plugin not installed'
            : e instanceof Error
              ? e.message
              : 'Failed to load hosts',
        )
      })
    return () => {
      cancelled = true
    }
  }, [widget.kind])

  const projectName = projects.find((p) => p.id === projectId)?.name
  const projectItems: MenuItem[] = [
    ...(spec.scope === 'optional'
      ? [
          {
            label: 'All projects',
            active: !projectId,
            testId: 'widget-config-project-all',
            onSelect: () => onPatch({ projectId: null }),
          },
        ]
      : []),
    ...projects.map((p) => ({
      label: p.name,
      hint: p.status === 'paused' ? 'paused' : undefined,
      searchText: p.id,
      active: p.id === projectId,
      testId: `widget-config-project-option-${p.id}`,
      onSelect: () => onPatch({ projectId: p.id }),
    })),
  ]

  const cardList = cards && cards.projectId === projectId ? cards.list : null
  const cardId = widget.cardId ?? null
  const cardItems: MenuItem[] = [
    {
      label: 'Whole project',
      active: !cardId,
      testId: 'widget-config-card-all',
      onSelect: () => onPatch({ cardId: null }),
    },
    ...(cardList ?? []).map((c) => ({
      label: c.title || 'Untitled card',
      hint: c.step,
      searchText: c.id,
      active: c.id === cardId,
      testId: `widget-config-card-option-${c.id}`,
      onSelect: () => onPatch({ cardId: c.id }),
    })),
  ]
  const cardLabel = cardId
    ? (cardList?.find((c) => c.id === cardId)?.title ?? cardTitle ?? 'Selected card')
    : 'Whole project'

  const reportRef = widget.reportRef ?? null
  const sortedReports = reports
    .slice()
    .sort((a, b) => (new Date(b.date).getTime() || 0) - (new Date(a.date).getTime() || 0))
  const reportItems: MenuItem[] = [
    {
      label: 'Latest report',
      description: 'Always shows the newest report',
      active: !reportRef,
      testId: 'widget-config-report-latest',
      onSelect: () => onPatch({ reportRef: null }),
    },
    ...sortedReports.map((r) => {
      const ref = `${r.folder}/${r.file}`
      return {
        label: r.title || r.file,
        hint: r.project_name || r.folder,
        searchText: ref,
        active: ref === reportRef,
        onSelect: () => onPatch({ reportRef: ref }),
      }
    }),
  ]
  const reportLabel = reportRef
    ? (reports.find((r) => `${r.folder}/${r.file}` === reportRef)?.title ?? reportRef)
    : 'Latest report'

  const hostRef = widget.hostRef ?? null
  const hostItems: MenuItem[] = [
    {
      label: 'All hosts',
      active: !hostRef,
      testId: 'widget-config-host-all',
      onSelect: () => onPatch({ hostRef: null }),
    },
    ...(hosts ?? []).map((h) => ({
      label: h.label || h.hostname,
      hint: `${h.username}@${h.hostname}`,
      searchText: h.id,
      active: h.id === hostRef,
      onSelect: () => onPatch({ hostRef: h.id }),
    })),
  ]
  const hostLabel = hostRef ? (hosts?.find((h) => h.id === hostRef)?.label ?? hostRef) : 'All hosts'

  return (
    <Modal
      onClose={onClose}
      maxWidth={460}
      className="dash-config-modal"
      data-testid="widget-config-modal"
    >
      <h2>Configure {spec.label}</h2>
      {spec.scope !== 'none' && (
        <div className="form-field">
          <label className="form-label" htmlFor="widget-config-project">
            Project
          </label>
          <Picker
            id="widget-config-project"
            testId="widget-config-project"
            items={projectItems}
            value={
              projectName ??
              (projectId
                ? 'Project'
                : spec.scope === 'optional'
                  ? 'All projects'
                  : 'Pick a project')
            }
            placeholder="Choose a project"
            emptyLabel={projectsLoaded ? 'No projects' : 'Loading projects…'}
            onOpen={() => void fetchProjects()}
          />
          {spec.scope === 'optional' && (
            <p className="form-hint">Limit this widget to one project, or show all of them.</p>
          )}
        </div>
      )}
      {widget.kind === 'dependencies' && (
        <div className="form-field">
          <label className="form-label" htmlFor="widget-config-card">
            Root card
          </label>
          <Picker
            id="widget-config-card"
            testId="widget-config-card"
            items={projectId ? cardItems : []}
            value={projectId ? cardLabel : 'Pick a project first'}
            placeholder="Choose a root card"
            emptyLabel={projectId ? 'Loading cards…' : 'Pick a project first'}
          />
          <p className="form-hint">Show only what this card depends on, or the whole project.</p>
        </div>
      )}
      {widget.kind === 'report' && (
        <div className="form-field">
          <label className="form-label" htmlFor="widget-config-report">
            Report
          </label>
          <Picker
            id="widget-config-report"
            testId="widget-config-report"
            items={reportItems}
            value={reportLabel}
            placeholder="Choose a report"
            emptyLabel={reportsLoading ? 'Loading reports…' : 'No reports'}
          />
        </div>
      )}
      {widget.kind === 'ssh_activity' && (
        <div className="form-field">
          <label className="form-label" htmlFor="widget-config-host">
            Host
          </label>
          <Picker
            id="widget-config-host"
            testId="widget-config-host"
            items={hostItems}
            value={hostLabel}
            placeholder="Choose a host"
            emptyLabel={hosts === null ? 'Loading hosts…' : 'No hosts'}
          />
          {hostsError && <p className="form-hint">{hostsError}</p>}
        </div>
      )}
      <div className="form-actions">
        <button
          type="button"
          className="btn-primary"
          onClick={onClose}
          data-testid="widget-config-done"
        >
          Done
        </button>
      </div>
    </Modal>
  )
}
