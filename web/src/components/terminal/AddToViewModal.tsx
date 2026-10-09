import { useEffect, useState, type FormEvent } from 'react'
import Modal from '../Modal'
import { MenuButton, type MenuItem } from '../Dropdown'
import { useViewsStore } from '../../store/views'
import { MAX_LEAVES, countLeaves, insertLeafAuto, terminalLeaf } from '../../lib/layoutTree'
import { describeActionError } from '../../utils/actionError'

type Target = { kind: 'existing'; id: string; name: string } | { kind: 'new' }

interface Props {
  terminalId: string
  onClose: () => void
  /** The terminal was added — open the view. */
  onAdded: (viewId: string) => void
}

/** "Add to view…": put a terminal in a pane of an existing saved view, or
 *  in a new view of its own. */
export default function AddToViewModal({ terminalId, onClose, onAdded }: Props) {
  const views = useViewsStore((s) => s.views)
  const fetchViews = useViewsStore((s) => s.fetchViews)
  const getView = useViewsStore((s) => s.getView)
  const updateView = useViewsStore((s) => s.updateView)
  const createView = useViewsStore((s) => s.createView)
  const [target, setTarget] = useState<Target | null>(null)
  const [name, setName] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')

  useEffect(() => {
    void fetchViews()
  }, [fetchViews])

  const disabledReason = !target
    ? 'Choose a view'
    : target.kind === 'new' && !name.trim()
      ? 'Enter a name'
      : ''

  const items: MenuItem[] = [
    { label: 'New view…', testId: 'add-to-view-new', onSelect: () => setTarget({ kind: 'new' }) },
    ...(views.length > 0 ? [{ divider: true }] : []),
    ...views.map((v) => ({
      label: v.name,
      searchText: v.id,
      testId: `add-to-view-option-${v.id}`,
      onSelect: () => setTarget({ kind: 'existing' as const, id: v.id, name: v.name }),
    })),
  ]

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    if (!target || disabledReason) return
    setBusy(true)
    setError('')
    try {
      if (target.kind === 'new') {
        const v = await createView(name.trim(), terminalLeaf(terminalId))
        onAdded(v.id)
        return
      }
      const v = await getView(target.id)
      if (countLeaves(v.layout) >= MAX_LEAVES) {
        throw new Error(`“${v.name}” already has ${MAX_LEAVES} panes`)
      }
      const aspect = window.innerHeight > 0 ? window.innerWidth / window.innerHeight : 16 / 9
      const blank = v.layout?.kind === 'leaf' && !v.layout.sessionId && !v.layout.terminalId
      await updateView(v.id, {
        layout: insertLeafAuto(blank ? null : v.layout, terminalLeaf(terminalId), aspect),
      })
      onAdded(v.id)
    } catch (err) {
      setError(describeActionError(err, "Couldn't add the terminal to the view."))
      setBusy(false)
    }
  }

  return (
    <Modal onClose={onClose} maxWidth={440} data-testid="add-to-view-modal">
      <h2>Add to View</h2>
      <form onSubmit={submit}>
        <div className="form-field">
          <label className="form-label" htmlFor="add-to-view-target">
            View
          </label>
          <MenuButton
            id="add-to-view-target"
            items={items}
            searchable
            searchPlaceholder="Search views…"
            searchTestId="add-to-view-search"
            listLabel="Views"
            haspopup="listbox"
            align="left"
            matchTriggerWidth
            ariaLabel="View"
            triggerClassName="form-input"
            testId="add-to-view-picker"
          >
            {!target ? 'Choose a view…' : target.kind === 'new' ? 'New view' : target.name}
          </MenuButton>
        </div>
        {target?.kind === 'new' && (
          <div className="form-field">
            <label className="form-label" htmlFor="add-to-view-name">
              Name
            </label>
            <input
              id="add-to-view-name"
              className="form-input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              maxLength={100}
              autoFocus
              data-testid="add-to-view-name"
            />
          </div>
        )}
        {error && (
          <p className="form-error" role="alert" data-testid="add-to-view-error">
            {error}
          </p>
        )}
        <div className="form-actions">
          {disabledReason && <span className="form-actions-reason">{disabledReason}</span>}
          <button type="button" className="btn-secondary" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={busy || !!disabledReason}
            data-testid="add-to-view-submit"
          >
            {busy ? 'Adding…' : 'Add'}
          </button>
        </div>
      </form>
    </Modal>
  )
}
