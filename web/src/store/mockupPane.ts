import { create } from 'zustand'

/** One mockup to show. `load` is called each time the pane (re)loads it, so a
 *  shared file can mint a fresh signed URL on Reload. */
export interface MockupDoc {
  title: string
  load: () => Promise<string>
}

interface MockupPaneState {
  /** Bumped on every open, so reopening a mockup always restarts it. */
  openId: number
  doc: MockupDoc | null
  open: (doc: MockupDoc) => void
  close: () => void
}

/** The right-hand mockup pane. Per-page-load UI state: there is one pane, and
 *  opening another mockup replaces what it shows. */
export const useMockupPaneStore = create<MockupPaneState>((set) => ({
  openId: 0,
  doc: null,
  open: (doc) => set((s) => ({ doc, openId: s.openId + 1 })),
  close: () => set({ doc: null }),
}))

/** Open a mockup whose HTML is already in hand (a fenced block). The title is
 *  the document's own `<title>` when it has one. */
export function openHtmlMockup(html: string): void {
  const title = html.match(/<title[^>]*>([^<]*)<\/title>/i)?.[1]?.trim() || 'HTML mockup'
  useMockupPaneStore.getState().open({ title, load: async () => html })
}
