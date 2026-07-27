import { useEffect, useState } from 'react'

// Delay before the banner appears, so the normal connect handshake on
// page load (and sub-second reconnect blips) never flash it.
const SHOW_DELAY_MS = 1500
// How long the "back online" announcement stays in the live region. Long
// enough for a screen reader to pick it up, short enough that a later
// disconnect reads as a fresh change.
const RESTORED_MS = 4000

/** `down` = the drop outlived the grace period; `restored` = it just ended. */
type Phase = 'quiet' | 'down' | 'restored'

/**
 * Connection status live region.
 *
 * The `role="status"` container is ALWAYS mounted and only its contents swap.
 * Most screen readers only announce mutations of a live region that already
 * existed when the change happened, so a region that enters the DOM together
 * with its text (the old behaviour here: `null` while connected, plus a
 * 1500ms delay before the text existed) is commonly announced as nothing at
 * all — exactly when the user needs to be told their input will not send.
 */
export default function ConnectionBanner({ connected }: { connected: boolean }) {
  const [phase, setPhase] = useState<Phase>('quiet')

  useEffect(() => {
    if (!connected) {
      const timer = window.setTimeout(() => setPhase('down'), SHOW_DELAY_MS)
      return () => window.clearTimeout(timer)
    }
    // Reconnected. Both transitions run off timers so the effect body never
    // calls setState synchronously (react-hooks/set-state-in-effect).
    // Recovery is only announced for a drop the user was actually told about.
    const settle = window.setTimeout(() => {
      setPhase((p) => (p === 'down' ? 'restored' : 'quiet'))
    }, 0)
    const expire = window.setTimeout(() => setPhase('quiet'), RESTORED_MS)
    return () => {
      window.clearTimeout(settle)
      window.clearTimeout(expire)
    }
  }, [connected])

  // Derived from the live prop, so a reconnect hides the banner on the same
  // commit rather than waiting for the settle timer.
  const showBanner = !connected && phase === 'down'
  const showRestored = connected && phase === 'restored'

  return (
    <div role="status" aria-live="polite" data-testid="connection-status">
      {showBanner && (
        <div className="connection-banner" data-testid="connection-banner">
          <span className="connection-banner-dot" aria-hidden="true" />
          Connection lost — reconnecting…
        </div>
      )}
      {showRestored && (
        <span className="sr-only" data-testid="connection-restored">
          Connection restored
        </span>
      )}
    </div>
  )
}
