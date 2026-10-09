import { StrictMode, type ReactNode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import App from './App.tsx'
import { initAppearance } from './util/appearance.ts'
import { initScrollIndicators } from './util/scrollIndicators.ts'
import ErrorBoundary from './components/ErrorBoundary.tsx'

// Apply persisted theme + accent hue before the first render so the
// saved appearance shows from the very first frame.
initAppearance()
// Scrollbars are hidden globally; this drives the edge-fade
// "more content" indicators on every scroll container.
initScrollIndicators()

const root = createRoot(document.getElementById('root')!)
const render = (node: ReactNode) =>
  root.render(
    <StrictMode>
      <ErrorBoundary label="app">{node}</ErrorBoundary>
    </StrictMode>,
  )

// `/terminal/<id>` is a terminal's pop-out window: the shell alone, no app
// chrome (see components/terminal/TerminalPopout.tsx). Loaded on demand so
// xterm.js never weighs on the main app bundle.
const popout = /^\/terminal\/([^/]+)\/?$/.exec(window.location.pathname)
if (popout) {
  void import('./components/terminal/TerminalPopout.tsx').then(({ default: TerminalPopout }) =>
    render(<TerminalPopout terminalId={decodeURIComponent(popout[1])} />),
  )
} else {
  render(<App />)
}

// Browsers refuse service workers on an untrusted cert (the self-signed
// HTTPS port reached by IP), even after the user clicks through the
// warning. The app works without one, so don't surface it as an error.
if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js').catch((err: unknown) => {
    console.info('Service worker not registered:', err)
  })
}
