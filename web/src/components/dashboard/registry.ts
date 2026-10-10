import type { ComponentType } from 'react'
import type { WidgetKind } from '../../store/views'
import type { InfoWidgetProps } from './widgets/types'
import NoteWidget from './widgets/NoteWidget'
import ReportWidget from './widgets/ReportWidget'
import BackgroundWidget from './widgets/BackgroundWidget'
import RepeatingWidget from './widgets/RepeatingWidget'
import TodosWidget from './widgets/TodosWidget'
import AttentionWidget from './widgets/AttentionWidget'
import ReviewQueueWidget from './widgets/ReviewQueueWidget'
import ReviewQualityWidget from './widgets/ReviewQualityWidget'
import WorkersWidget from './widgets/WorkersWidget'
import WorktreesWidget from './widgets/WorktreesWidget'
import DependenciesWidget from './widgets/DependenciesWidget'
import SshActivityWidget from './widgets/SshActivityWidget'
import SshHostsWidget from './widgets/SshHostsWidget'
import PrsWidget from './widgets/PrsWidget'
import OrchestratorsWidget from './widgets/OrchestratorsWidget'

export type WidgetCategory = 'panes' | 'project' | 'activity' | 'quality' | 'notes' | 'infra'

/** Add-widget menu sections, in order. */
export const WIDGET_CATEGORIES: { id: WidgetCategory; label: string }[] = [
  { id: 'panes', label: 'Panes' },
  { id: 'project', label: 'Project' },
  { id: 'activity', label: 'Activity' },
  { id: 'quality', label: 'Quality' },
  { id: 'notes', label: 'Notes' },
  { id: 'infra', label: 'Infrastructure' },
]

/** How a kind uses `projectId`: not at all, an optional scope (null = all
 *  projects), or a required target (null = picker placeholder). */
export type ScopeMode = 'none' | 'optional' | 'required'

export interface WidgetSpec {
  kind: WidgetKind
  /** Add-menu label and default title. */
  label: string
  /** Muted add-menu hint. */
  hint: string
  category: WidgetCategory
  /** Default footprint in grid cells (contract table). */
  size: { w: number; h: number }
  scope: ScopeMode
  /** Offers "Configure…" (scope / report / root card / host). */
  configurable: boolean
  /** Info widgets render this; panes (session / terminal / project) are
   *  rendered by the page, which owns their live state. */
  component?: ComponentType<InfoWidgetProps>
  /** Plugin the kind needs installed; hidden from the Add menu otherwise. */
  plugin?: string
}

/** Every dashboard widget kind, in Add-menu order within its category. */
export const WIDGET_SPECS: Record<WidgetKind, WidgetSpec> = {
  session: {
    kind: 'session',
    label: 'Session',
    hint: 'live chat',
    category: 'panes',
    size: { w: 6, h: 10 },
    scope: 'none',
    configurable: false,
  },
  terminal: {
    kind: 'terminal',
    label: 'Terminal',
    hint: 'remote shell',
    category: 'panes',
    size: { w: 6, h: 10 },
    scope: 'none',
    configurable: false,
  },
  project: {
    kind: 'project',
    label: 'Project',
    hint: 'board summary',
    category: 'project',
    size: { w: 4, h: 8 },
    scope: 'required',
    configurable: false,
  },
  todos: {
    kind: 'todos',
    label: 'Todos',
    hint: 'worker checklists',
    category: 'project',
    size: { w: 4, h: 8 },
    scope: 'required',
    configurable: true,
    component: TodosWidget,
  },
  dependencies: {
    kind: 'dependencies',
    label: 'Dependencies',
    hint: 'card graph',
    category: 'project',
    size: { w: 6, h: 9 },
    scope: 'required',
    configurable: true,
    component: DependenciesWidget,
  },
  attention: {
    kind: 'attention',
    label: 'Needs Attention',
    hint: 'questions, blocks',
    category: 'activity',
    size: { w: 4, h: 8 },
    scope: 'optional',
    configurable: true,
    component: AttentionWidget,
  },
  workers: {
    kind: 'workers',
    label: 'Workers',
    hint: 'running agents',
    category: 'activity',
    size: { w: 6, h: 7 },
    scope: 'optional',
    configurable: true,
    component: WorkersWidget,
  },
  background: {
    kind: 'background',
    label: 'Background Tasks',
    hint: 'long-running jobs',
    category: 'activity',
    size: { w: 6, h: 6 },
    scope: 'none',
    configurable: false,
    component: BackgroundWidget,
  },
  repeating: {
    kind: 'repeating',
    label: 'Repeating Tasks',
    hint: 'schedules',
    category: 'activity',
    size: { w: 4, h: 6 },
    scope: 'none',
    configurable: false,
    component: RepeatingWidget,
  },
  orchestrators: {
    kind: 'orchestrators',
    label: 'Orchestrators',
    hint: 'goals + runs',
    category: 'activity',
    size: { w: 6, h: 8 },
    scope: 'none',
    configurable: false,
    component: OrchestratorsWidget,
    plugin: 'session-control',
  },
  review_queue: {
    kind: 'review_queue',
    label: 'Review Queue',
    hint: 'in review + verdicts',
    category: 'quality',
    size: { w: 4, h: 8 },
    scope: 'optional',
    configurable: true,
    component: ReviewQueueWidget,
  },
  review_quality: {
    kind: 'review_quality',
    label: 'Review Quality',
    hint: 'pass rate trend',
    category: 'quality',
    size: { w: 6, h: 7 },
    scope: 'optional',
    configurable: true,
    component: ReviewQualityWidget,
  },
  worktrees: {
    kind: 'worktrees',
    label: 'Worktrees',
    hint: 'unmerged + commits',
    category: 'quality',
    size: { w: 6, h: 7 },
    scope: 'optional',
    configurable: true,
    component: WorktreesWidget,
  },
  prs: {
    kind: 'prs',
    label: 'PRs & CI',
    hint: 'linked pull requests',
    category: 'quality',
    size: { w: 6, h: 8 },
    scope: 'optional',
    configurable: true,
    component: PrsWidget,
    plugin: 'github-bridge',
  },
  note: {
    kind: 'note',
    label: 'Note',
    hint: 'markdown',
    category: 'notes',
    size: { w: 4, h: 6 },
    scope: 'none',
    configurable: false,
    component: NoteWidget,
  },
  report: {
    kind: 'report',
    label: 'Report',
    hint: 'latest or pinned',
    category: 'notes',
    size: { w: 6, h: 10 },
    scope: 'none',
    configurable: true,
    component: ReportWidget,
  },
  ssh_activity: {
    kind: 'ssh_activity',
    label: 'SSH Activity',
    hint: 'commands on hosts',
    category: 'infra',
    size: { w: 6, h: 8 },
    scope: 'none',
    configurable: true,
    component: SshActivityWidget,
    plugin: 'ssh-fleet',
  },
  ssh_hosts: {
    kind: 'ssh_hosts',
    label: 'SSH Hosts',
    hint: 'fleet status',
    category: 'infra',
    size: { w: 4, h: 7 },
    scope: 'none',
    configurable: false,
    component: SshHostsWidget,
    plugin: 'ssh-fleet',
  },
}

/** Kinds the page renders itself (live panes and the project summary). */
export function isPane(kind: WidgetKind): kind is 'session' | 'terminal' | 'project' {
  return kind === 'session' || kind === 'terminal' || kind === 'project'
}

/** Add-menu test id for a kind: `view-add-review-queue`. */
export function addTestId(kind: WidgetKind): string {
  return `view-add-${kind.replace(/_/g, '-')}`
}
