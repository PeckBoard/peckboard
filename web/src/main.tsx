import { StrictMode } from 'react'
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

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <ErrorBoundary label="app">
      <App />
    </ErrorBoundary>
  </StrictMode>,
)

if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js')
}
