import { useEffect, useState } from 'react'
import { useVoiceStore, type AssistantInfo } from '../../store/voice'
import { MenuButton, type MenuItem } from '../Dropdown'
import AssistantDialog from './AssistantDialog'

/** How often the other assistants' "something waiting" counts refresh. */
const POLL_MS = 10_000

/** What a row says after the name: default, working, updates waiting. */
function rowHint(a: AssistantInfo): string | undefined {
  const parts = [
    a.is_default ? 'Default' : '',
    a.working ? 'Working' : '',
    a.waiting > 0 ? `${a.waiting} waiting` : '',
  ].filter(Boolean)
  return parts.length > 0 ? parts.join(' · ') : undefined
}

/**
 * The Assistant page's "who am I talking to" chip: shows the active
 * assistant, switches between them, and opens the add / edit dialog. A
 * count on the chip says how many updates the OTHER assistants are holding
 * (a scheduled task that came due, a hand-off), since only the active one
 * speaks.
 */
export default function AssistantSwitcher() {
  const assistants = useVoiceStore((s) => s.assistants)
  const assistantId = useVoiceStore((s) => s.assistantId)
  const max = useVoiceStore((s) => s.maxAssistants)
  const switchAssistant = useVoiceStore((s) => s.switchAssistant)
  const roomMode = useVoiceStore((s) => s.roomMode)
  const setRoomMode = useVoiceStore((s) => s.setRoomMode)
  // `undefined` = closed, `null` = adding, an assistant = editing it.
  const [dialog, setDialog] = useState<AssistantInfo | null | undefined>(undefined)

  useEffect(() => {
    const load = () => void useVoiceStore.getState().loadAssistants()
    load()
    const timer = setInterval(load, POLL_MS)
    return () => clearInterval(timer)
  }, [])

  const active = assistants.find((a) => a.id === assistantId) ?? null
  const full = assistants.length >= max
  const waitingElsewhere = assistants
    .filter((a) => a.id !== assistantId)
    .reduce((n, a) => n + a.waiting, 0)

  const items: MenuItem[] = [
    ...assistants.map(
      (a): MenuItem => ({
        label: a.name,
        hint: rowHint(a),
        description: a.job ? a.job.split('\n')[0] : undefined,
        active: a.id === assistantId,
        onSelect: () => void switchAssistant(a.id),
        testId: `assistant-switch-${a.name}`,
      }),
    ),
    { divider: true, hidden: assistants.length < 2 },
    {
      label: 'Room: everyone joins in',
      description:
        'Any assistant can speak up when it has news. Say "everyone, …" to ask them all in turn.',
      hint: roomMode ? 'On' : 'Off',
      active: roomMode,
      hidden: assistants.length < 2,
      onSelect: () => setRoomMode(!roomMode),
      testId: 'assistant-room-toggle',
    },
    { divider: true },
    {
      label: 'Add assistant…',
      hint: full ? `Limit of ${max} reached` : undefined,
      disabled: full,
      onSelect: () => setDialog(null),
      testId: 'assistant-add',
    },
    {
      label: active ? `Edit ${active.name}…` : 'Edit…',
      hidden: !active,
      onSelect: () => setDialog(active),
      testId: 'assistant-edit',
    },
  ]

  return (
    <>
      <MenuButton
        items={items}
        ariaLabel={
          (active ? `Assistant: ${active.name}. Switch assistant` : 'Switch assistant') +
          (waitingElsewhere > 0 ? `. ${waitingElsewhere} waiting with other assistants` : '')
        }
        triggerClassName="assistant-chip assistant-chip-button assistant-switcher"
        testId="assistant-switcher"
        align="right"
        haspopup="menu"
      >
        <span
          className="assistant-switcher-dot"
          style={active?.color ? { background: active.color } : undefined}
          aria-hidden
        />
        <span data-testid="assistant-switcher-name">{active?.name ?? 'Assistant'}</span>
        {waitingElsewhere > 0 && (
          <span
            className="assistant-chip-count"
            data-testid="assistant-switcher-waiting"
            title="Updates waiting with your other assistants"
          >
            {waitingElsewhere}
          </span>
        )}
        <svg className="assistant-switcher-caret" viewBox="0 0 12 12" aria-hidden>
          <path d="M2.5 4.5 6 8l3.5-3.5" />
        </svg>
      </MenuButton>
      {dialog !== undefined && (
        <AssistantDialog
          key={dialog?.id ?? 'new'}
          assistant={dialog}
          onClose={() => setDialog(undefined)}
        />
      )}
    </>
  )
}
