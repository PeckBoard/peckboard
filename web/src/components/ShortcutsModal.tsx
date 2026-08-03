import Modal from './Modal'

// `navigator.platform` is deprecated but still the reliable way to pick the
// modifier glyph without a UA-string parse; fall back to Ctrl on unknowns.
const isApple = /Mac|iP(hone|ad|od)/.test(navigator.platform)
const MOD = isApple ? '⌘' : 'Ctrl'

// `sep` is the word rendered between keys — '+' for a chord, 'then' for a
// sequence pressed one key after the other.
const SHORTCUTS: { keys: string[]; action: string; sep?: string }[] = [
  { keys: [MOD, 'K'], action: 'Search sessions' },
  { keys: ['N'], action: 'New session' },
  // NOT Cmd/Ctrl+1…9 — browsers reserve those for their own tab strip and
  // never deliver the keydown to the page. See the note in TabBar.tsx.
  { keys: ['G', '1…9'], sep: 'then', action: 'Switch to the nth open tab' },
  { keys: [MOD, 'F'], action: 'Search the open chat transcript' },
  { keys: ['?'], action: 'Show this cheat sheet' },
]

/** Keyboard-shortcuts cheat sheet, opened app-wide with `?`. */
export default function ShortcutsModal({ onClose }: { onClose: () => void }) {
  return (
    <Modal onClose={onClose} maxWidth={400} data-testid="shortcuts-modal">
      <h2>Keyboard shortcuts</h2>
      <dl className="shortcuts-list">
        {SHORTCUTS.map((s) => (
          <div key={s.action} className="shortcuts-row">
            <dt className="shortcuts-keys">
              {s.keys.map((k, i) => (
                <span key={k}>
                  {i > 0 && (
                    <span className="shortcuts-sep" aria-hidden="true">
                      {s.sep ?? '+'}
                    </span>
                  )}
                  <kbd>{k}</kbd>
                </span>
              ))}
            </dt>
            <dd className="shortcuts-action">{s.action}</dd>
          </div>
        ))}
      </dl>
    </Modal>
  )
}
