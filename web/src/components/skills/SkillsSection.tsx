import { useEffect, useState } from 'react'
import List from '../List'
import EmptyState from '../EmptyState'
import ConfirmDialog from '../ConfirmDialog'
import HelpTip from '../HelpTip'
import SkillEditorModal from './SkillEditorModal'
import SkillDetailModal from './SkillDetailModal'
import SkillPublishModal from './SkillPublishModal'
import SkillsBrowseTab from './SkillsBrowseTab'
import SkillGroupsTab from './SkillGroupsTab'
import SkillSourcesTab from './SkillSourcesTab'
import TeamMemoryTab from './TeamMemoryTab'
import { inUseVersion, SCOPE_LABEL, useSkillsStore, type Skill } from '../../store/skills'
import type { MenuItem } from '../Dropdown'

type Tab = 'installed' | 'browse' | 'groups' | 'sources' | 'memory'

const TABS: { id: Tab; label: string }[] = [
  { id: 'installed', label: 'Installed' },
  { id: 'browse', label: 'Browse' },
  { id: 'groups', label: 'Groups' },
  { id: 'sources', label: 'Sources' },
  { id: 'memory', label: 'Team memory' },
]

/** `?subscribe=<name>` — the peckboard.com "Add to Peckboard" link. */
function subscribeParam(): string | null {
  return new URLSearchParams(window.location.search).get('subscribe')
}

/**
 * Settings → Skills. Installed (local skills + subscriptions), Browse (the
 * catalog across sources), Groups, Sources and Team memory, one tab each.
 */
export default function SkillsSection() {
  const [deepLink] = useState(subscribeParam)
  const [tab, setTab] = useState<Tab>(deepLink ? 'browse' : 'installed')
  const fetchAll = useSkillsStore((s) => s.fetchAll)

  useEffect(() => {
    void fetchAll()
  }, [fetchAll])

  return (
    <div className="registry-panel-page" data-testid="settings-skills">
      <p className="form-hint">
        Skills <HelpTip term="skill" /> are reusable instructions an agent loads when a job calls
        for them. Write your own, subscribe to shared ones, and give sessions the ones they need.
      </p>
      <div className="registry-tabs" role="tablist">
        {TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            aria-selected={tab === t.id}
            className={`registry-tab ${tab === t.id ? 'registry-tab--active' : ''}`}
            data-testid={`skills-tab-${t.id}`}
            onClick={() => setTab(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>
      {tab === 'installed' && <InstalledTab onBrowse={() => setTab('browse')} />}
      {tab === 'browse' && <SkillsBrowseTab initialName={deepLink} />}
      {tab === 'groups' && <SkillGroupsTab />}
      {tab === 'sources' && <SkillSourcesTab />}
      {tab === 'memory' && <TeamMemoryTab onAddSource={() => setTab('sources')} />}
    </div>
  )
}

function InstalledTab({ onBrowse }: { onBrowse: () => void }) {
  const skills = useSkillsStore((s) => s.skills)
  const loaded = useSkillsStore((s) => s.skillsLoaded)
  const error = useSkillsStore((s) => s.error)
  const fetchSkills = useSkillsStore((s) => s.fetchSkills)
  const deleteSkill = useSkillsStore((s) => s.deleteSkill)
  const unsubscribe = useSkillsStore((s) => s.unsubscribe)
  const [query, setQuery] = useState('')
  const [editing, setEditing] = useState<Skill | 'new' | null>(null)
  const [openId, setOpenId] = useState<string | null>(null)
  const [confirm, setConfirm] = useState<Skill | null>(null)
  const [publishing, setPublishing] = useState<Skill | null>(null)
  const [busy, setBusy] = useState(false)
  const [confirmError, setConfirmError] = useState<string | null>(null)

  const q = query.trim().toLowerCase()
  const visible = skills.filter(
    (s) =>
      !q ||
      `${s.name} ${s.description} ${s.tags.join(' ')} ${s.groups.join(' ')}`
        .toLowerCase()
        .includes(q),
  )
  // Read the live row so a pin change in the open modal shows immediately.
  const open = skills.find((s) => s.id === openId) ?? null

  const menu = (s: Skill): MenuItem[] =>
    s.subscribed
      ? [
          { label: 'Open', onSelect: () => setOpenId(s.id) },
          { divider: true },
          { label: 'Unsubscribe', danger: true, onSelect: () => setConfirm(s) },
        ]
      : [
          { label: 'Open', onSelect: () => setOpenId(s.id) },
          { label: 'Edit', onSelect: () => setEditing(s), testId: `skill-edit-${s.name}` },
          ...(s.scope === 'local'
            ? [
                {
                  label: 'Publish…',
                  onSelect: () => setPublishing(s),
                  testId: `skill-publish-${s.name}`,
                },
              ]
            : []),
          { divider: true },
          { label: 'Delete', danger: true, onSelect: () => setConfirm(s) },
        ]

  return (
    <div className="skills-tab" data-testid="skills-installed">
      <div className="registry-controls">
        <input
          type="search"
          className="registry-search"
          placeholder="Search installed skills…"
          aria-label="Search installed skills"
          data-testid="skills-search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <button
          type="button"
          className="btn-primary btn-sm"
          onClick={() => setEditing('new')}
          data-testid="skill-new"
        >
          + New skill
        </button>
      </div>
      {error && (
        <div className="form-error" role="alert">
          <span>{error}</span>{' '}
          <button type="button" className="btn-secondary" onClick={() => void fetchSkills()}>
            Retry
          </button>
        </div>
      )}
      <List<Skill>
        items={visible}
        getKey={(s) => s.id}
        bodyClassName="list-view-rows"
        onActivate={(s) => setOpenId(s.id)}
        getMenuItems={menu}
        renderItem={(s) => (
          <>
            <span className="list-view-name" data-testid={`skill-row-${s.name}`}>
              {s.name}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag">{SCOPE_LABEL[s.scope]}</span>
              <span>
                v{inUseVersion(s)}
                {s.subscribed ? (s.pinned_version ? ' (pinned)' : ' (latest)') : ''}
              </span>
              {s.tags.map((t) => (
                <span key={t} className="list-view-tag">
                  #{t}
                </span>
              ))}
              <span className="skill-row-desc">{s.description}</span>
            </span>
          </>
        )}
        emptyState={
          !loaded ? (
            <div className="list-view-empty">Loading…</div>
          ) : skills.length === 0 ? (
            <EmptyState
              title="No skills yet"
              body="Write a skill for something you explain to agents again and again, or browse the shared catalog."
              actionLabel="+ New skill"
              onAction={() => setEditing('new')}
              term="skill"
              testId="skills-installed-empty"
            >
              <button type="button" className="form-link-btn" onClick={onBrowse}>
                Browse the catalog
              </button>
            </EmptyState>
          ) : (
            <div className="list-view-empty">No installed skills match.</div>
          )
        }
      />
      {editing && (
        <SkillEditorModal
          skill={editing === 'new' ? undefined : editing}
          onClose={() => setEditing(null)}
        />
      )}
      {open && (
        <SkillDetailModal
          skill={open}
          onClose={() => setOpenId(null)}
          onEdit={
            open.subscribed
              ? undefined
              : () => {
                  setOpenId(null)
                  setEditing(open)
                }
          }
          onPublish={
            open.scope === 'local'
              ? () => {
                  setOpenId(null)
                  setPublishing(open)
                }
              : undefined
          }
        />
      )}
      {publishing && <SkillPublishModal skill={publishing} onClose={() => setPublishing(null)} />}
      {confirm && (
        <ConfirmDialog
          title={confirm.subscribed ? 'Unsubscribe' : 'Delete skill'}
          message={
            confirm.subscribed
              ? `Unsubscribe from "${confirm.name}"? Sessions using it lose it on their next start. You can subscribe again from Browse.`
              : `Delete "${confirm.name}" and every version of it? Sessions using it lose it on their next start. This cannot be undone.`
          }
          confirmLabel={confirm.subscribed ? 'Unsubscribe' : 'Delete'}
          danger
          busy={busy}
          error={confirmError}
          testId="skill-delete-confirm"
          onConfirm={() => {
            setBusy(true)
            setConfirmError(null)
            ;(confirm.subscribed ? unsubscribe(confirm.id) : deleteSkill(confirm.id))
              .then(() => setConfirm(null))
              .catch((e: Error) => setConfirmError(e.message))
              .finally(() => setBusy(false))
          }}
          onCancel={() => {
            if (busy) return
            setConfirm(null)
            setConfirmError(null)
          }}
        />
      )}
    </div>
  )
}
