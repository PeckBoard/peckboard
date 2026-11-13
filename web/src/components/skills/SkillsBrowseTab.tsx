import { useEffect, useMemo, useRef, useState } from 'react'
import List from '../List'
import Modal from '../Modal'
import EmptyState from '../EmptyState'
import { SCOPE_LABEL, searchCatalog, useSkillsStore, type CatalogSkill } from '../../store/skills'

const keyOf = (s: CatalogSkill) => `${s.source_id ?? 'local'}:${s.name}`

interface Props {
  /** Skill name from a `?subscribe=<name>` deep link: prefilled as the
   *  search and opened once it shows up in the results. */
  initialName?: string | null
}

/**
 * The browse catalog: every skill the configured sources publish (the public
 * index and each team repo) plus local ones. Search is server-side; source,
 * group and tag narrow the results. Picking a row opens it, where the user
 * subscribes to the latest or a specific version.
 */
export default function SkillsBrowseTab({ initialName }: Props) {
  const sources = useSkillsStore((s) => s.sources)
  const [query, setQuery] = useState(initialName ?? '')
  const [sourceId, setSourceId] = useState('')
  const [group, setGroup] = useState('')
  const [tag, setTag] = useState('')
  const [items, setItems] = useState<CatalogSkill[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [sourceErrors, setSourceErrors] = useState<{ source_id: string; error: string }[]>([])
  const [openKey, setOpenKey] = useState<string | null>(null)
  // Deep link: open the named skill the first time it is in the results.
  const pendingDeepLink = useRef(initialName ?? null)
  const [reloadTick, setReloadTick] = useState(0)

  useEffect(() => {
    let cancelled = false
    const t = setTimeout(() => {
      searchCatalog(query.trim(), sourceId || undefined)
        .then(({ items: res, errors }) => {
          if (cancelled) return
          setItems(res)
          setSourceErrors(errors)
          setError(null)
          const hit = res.find((s) => s.name === pendingDeepLink.current)
          if (hit) {
            pendingDeepLink.current = null
            setOpenKey(keyOf(hit))
          }
        })
        .catch((e: Error) => !cancelled && setError(e.message))
    }, 200)
    return () => {
      cancelled = true
      clearTimeout(t)
    }
  }, [query, sourceId, reloadTick])

  const allGroups = useMemo(
    () => [...new Set((items ?? []).flatMap((s) => s.groups))].sort(),
    [items],
  )
  const allTags = useMemo(() => [...new Set((items ?? []).flatMap((s) => s.tags))].sort(), [items])
  const visible = (items ?? []).filter(
    (s) => (!group || s.groups.includes(group)) && (!tag || s.tags.includes(tag)),
  )
  const sourceName = (id: string | null) =>
    id ? (sources.find((x) => x.id === id)?.name ?? 'Source') : 'This server'
  const open = items?.find((s) => keyOf(s) === openKey) ?? null

  return (
    <div className="skills-tab" data-testid="skills-browse">
      <div className="registry-controls">
        <input
          type="search"
          className="registry-search"
          placeholder="Search skills…"
          aria-label="Search skills"
          data-testid="skills-search"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        <select
          className="plugin-setting-select"
          aria-label="Source"
          data-testid="skills-filter-source"
          value={sourceId}
          onChange={(e) => setSourceId(e.target.value)}
        >
          <option value="">All sources</option>
          <option value="local">This server</option>
          {sources.map((s) => (
            <option key={s.id} value={s.id}>
              {s.name}
            </option>
          ))}
        </select>
        <select
          className="plugin-setting-select"
          aria-label="Group"
          data-testid="skills-filter-group"
          value={group}
          onChange={(e) => setGroup(e.target.value)}
        >
          <option value="">All groups</option>
          {allGroups.map((g) => (
            <option key={g} value={g}>
              {g}
            </option>
          ))}
        </select>
        <select
          className="plugin-setting-select"
          aria-label="Tag"
          data-testid="skills-filter-tag"
          value={tag}
          onChange={(e) => setTag(e.target.value)}
        >
          <option value="">All tags</option>
          {allTags.map((t) => (
            <option key={t} value={t}>
              #{t}
            </option>
          ))}
        </select>
      </div>
      {error && (
        <div className="form-error" data-testid="skills-browse-error">
          <span>{error}</span>{' '}
          <button
            type="button"
            className="btn-secondary"
            onClick={() => setReloadTick((n) => n + 1)}
          >
            Retry
          </button>
        </div>
      )}
      {sourceErrors.map((e) => (
        <p key={e.source_id} className="form-hint" data-testid="skills-browse-source-error">
          Couldn&rsquo;t read {sourceName(e.source_id)}: {e.error}
        </p>
      ))}
      <List<CatalogSkill>
        items={visible}
        getKey={keyOf}
        bodyClassName="list-view-rows"
        onActivate={(s) => setOpenKey(keyOf(s))}
        renderItem={(s) => (
          <>
            <span className="list-view-name" data-testid={`skill-row-${s.name}`}>
              {s.name}
            </span>
            <span className="list-view-meta">
              <span className="list-view-tag">{SCOPE_LABEL[s.scope]}</span>
              {s.subscribed && <span className="list-view-tag">Subscribed</span>}
              <span>v{s.available_versions[0] ?? s.version}</span>
              <span>{sourceName(s.source_id)}</span>
              <span className="skill-row-desc">{s.description}</span>
            </span>
          </>
        )}
        emptyState={
          items === null && !error ? (
            <div className="list-view-empty">Loading…</div>
          ) : (items ?? []).length === 0 && !query && !sourceId ? (
            <EmptyState
              title="Nothing to browse yet"
              body="Skills from the public catalog and your team repositories show up here. Add a team repository on the Sources tab, or sync the public source."
              term="skill"
              testId="skills-browse-empty"
            />
          ) : (
            <div className="list-view-empty" data-testid="skills-browse-nomatch">
              No skills match.
            </div>
          )
        }
      />
      {open && (
        <CatalogDetailModal
          skill={open}
          sourceLabel={sourceName(open.source_id)}
          onClose={() => setOpenKey(null)}
          onSubscribed={() => setReloadTick((n) => n + 1)}
        />
      )}
    </div>
  )
}

function CatalogDetailModal({
  skill,
  sourceLabel,
  onClose,
  onSubscribed,
}: {
  skill: CatalogSkill
  sourceLabel: string
  onClose: () => void
  onSubscribed: () => void
}) {
  const subscribe = useSkillsStore((s) => s.subscribe)
  const [version, setVersion] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const versions = skill.available_versions.length > 0 ? skill.available_versions : [skill.version]
  const canSubscribe = !!skill.source_id && !skill.subscribed

  const doSubscribe = () => {
    if (!skill.source_id) return
    setBusy(true)
    setError(null)
    subscribe(skill.source_id, skill.name, version || null)
      .then(() => {
        onSubscribed()
        onClose()
      })
      .catch((e: Error) => setError(e.message))
      .finally(() => setBusy(false))
  }

  return (
    <Modal onClose={onClose} maxWidth={560} data-testid="skill-catalog-modal">
      <h2>{skill.name}</h2>
      <div className="skill-detail-meta">
        <span className="list-view-tag">{SCOPE_LABEL[skill.scope]}</span>
        <span className="list-view-tag">{sourceLabel}</span>
        {skill.tags.map((t) => (
          <span key={t} className="list-view-tag">
            #{t}
          </span>
        ))}
        {skill.groups.map((g) => (
          <span key={g} className="list-view-tag">
            group: {g}
          </span>
        ))}
      </div>
      {skill.description && <p className="form-hint">{skill.description}</p>}
      {skill.source_id && (
        <div className="form-field">
          <label className="form-label" htmlFor="skill-catalog-version">
            Version
          </label>
          <select
            id="skill-catalog-version"
            className="form-input"
            value={version}
            disabled={!canSubscribe}
            onChange={(e) => setVersion(e.target.value)}
            data-testid="skill-version-select"
          >
            <option value="">Latest (v{versions[0]}) — follow new releases</option>
            {versions.map((v) => (
              <option key={v} value={v}>
                v{v} — stay on this version
              </option>
            ))}
          </select>
        </div>
      )}
      {error && (
        <p className="form-error" role="alert" data-testid="skill-subscribe-error">
          {error}
        </p>
      )}
      <div className="form-actions">
        {!canSubscribe && (
          <span className="form-actions-reason" data-testid="skill-subscribe-reason">
            {skill.subscribed
              ? 'Already subscribed — change its version under Installed'
              : 'A local skill is already installed'}
          </span>
        )}
        <button type="button" className="btn-secondary" onClick={onClose}>
          Close
        </button>
        <button
          type="button"
          className="btn-primary"
          disabled={!canSubscribe || busy}
          onClick={doSubscribe}
          data-testid="skill-subscribe"
        >
          {busy ? 'Subscribing…' : 'Subscribe'}
        </button>
      </div>
    </Modal>
  )
}
