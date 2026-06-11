import { useEffect, useState } from 'react'

// Delay before the banner appears, so the normal connect handshake on
// page load (and sub-second reconnect blips) never flash it.
const SHOW_DELAY_MS = 1500

export default function ConnectionBanner({ connected }: { connected: boolean }) {
  // Unmounting on reconnect resets the delay timer naturally, so the
  // next disconnect starts a fresh grace period.
  if (connected) return null
  return <DelayedBanner />
}

function DelayedBanner() {
  const [visible, setVisible] = useState(false)

  useEffect(() => {
    const timer = window.setTimeout(() => setVisible(true), SHOW_DELAY_MS)
    return () => window.clearTimeout(timer)
  }, [])

  if (!visible) return null

  return (
    <div className="connection-banner" role="status" data-testid="connection-banner">
      <span className="connection-banner-dot" aria-hidden="true" />
      Connection lost — reconnecting…
    </div>
  )
}
