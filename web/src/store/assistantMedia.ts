import { create } from 'zustand'

/**
 * What the voice assistant is showing above the orb on the Assistant page,
 * driven by the `assistant-media` WS frame (`show_media`, `media_control`,
 * `hide_media`). Browser-only on purpose: a reload closes the embed.
 */

export type AssistantMediaKind = 'image' | 'video' | 'model'

export type AssistantMediaSource =
  | { type: 'file'; sessionId: string; id: string; name: string }
  | { type: 'url'; url: string }

export interface AssistantMediaItem {
  /** New per `show_media`, so showing the same file twice restarts it. */
  key: number
  kind: AssistantMediaKind
  title: string
  source: AssistantMediaSource
}

export type AssistantMediaCommand =
  | { action: 'play' | 'pause' | 'mute' | 'unmute' | 'reset' }
  | { action: 'seek'; seconds: number }
  | { action: 'seek'; by: number }
  | { action: 'rotate'; degrees: number; axis: 'vertical' | 'horizontal' }
  | { action: 'zoom'; factor: number }

export type SequencedCommand = AssistantMediaCommand & { seq: number }

interface AssistantMediaState {
  media: AssistantMediaItem | null
  /** Commands for `media`, in order; a player applies the ones it hasn't. */
  commands: SequencedCommand[]
  applyFrame: (data: unknown) => void
  hide: () => void
}

function parseSource(d: Record<string, unknown>): AssistantMediaSource | null {
  if (typeof d.url === 'string' && /^https?:\/\//i.test(d.url)) {
    return { type: 'url', url: d.url }
  }
  const file = d.file as Record<string, unknown> | undefined
  if (
    file &&
    typeof file.session_id === 'string' &&
    typeof file.id === 'string' &&
    typeof file.name === 'string'
  ) {
    return { type: 'file', sessionId: file.session_id, id: file.id, name: file.name }
  }
  return null
}

function parseCommand(c: unknown): AssistantMediaCommand | null {
  if (!c || typeof c !== 'object') return null
  const d = c as Record<string, unknown>
  const num = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : null)
  switch (d.action) {
    case 'play':
    case 'pause':
    case 'mute':
    case 'unmute':
    case 'reset':
      return { action: d.action }
    case 'seek': {
      const seconds = num(d.seconds)
      if (seconds !== null) return { action: 'seek', seconds }
      const by = num(d.by)
      return by === null ? null : { action: 'seek', by }
    }
    case 'rotate': {
      const degrees = num(d.degrees)
      if (degrees === null) return null
      return {
        action: 'rotate',
        degrees,
        axis: d.axis === 'horizontal' ? 'horizontal' : 'vertical',
      }
    }
    case 'zoom': {
      const factor = num(d.factor)
      return factor === null || factor <= 0 ? null : { action: 'zoom', factor }
    }
    default:
      return null
  }
}

let nextKey = 1
let nextSeq = 1

export const useAssistantMedia = create<AssistantMediaState>((set, get) => ({
  media: null,
  commands: [],
  applyFrame: (data) => {
    if (!data || typeof data !== 'object') return
    const d = data as Record<string, unknown>
    if (d.action === 'hide') {
      get().hide()
      return
    }
    if (d.action === 'control') {
      const command = parseCommand(d.command)
      if (command && get().media) {
        set((s) => ({ commands: [...s.commands, { ...command, seq: nextSeq++ }] }))
      }
      return
    }
    if (d.action !== 'show') return
    const source = parseSource(d)
    if (!source) return
    if (d.kind !== 'image' && d.kind !== 'video' && d.kind !== 'model') return
    set({
      media: {
        key: nextKey++,
        kind: d.kind,
        title: typeof d.title === 'string' ? d.title : '',
        source,
      },
      commands: [],
    })
  },
  hide: () => set({ media: null, commands: [] }),
}))
