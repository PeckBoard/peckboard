import { useEffect, useRef, useState } from 'react'

import './MermaidBlock.css'

let mermaidPromise: Promise<typeof import('mermaid').default> | null = null
let idSeq = 0

/** Lazy-load mermaid only when a document holding a ```mermaid block is
 *  opened — a plan, a chat reply, a review — so it never weighs down the main
 *  bundle. Renders the diagram to SVG; on any parse/render error it falls back
 *  to showing the raw source, so the passage is never blank. */
export default function MermaidBlock({ code }: { code: string }) {
  const ref = useRef<HTMLDivElement>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    if (!mermaidPromise) {
      mermaidPromise = import('mermaid').then((m) => {
        // `base` + explicit variables instead of the stock `default` theme:
        // the stock palette rendered near-black nodes in the embedded
        // browser, and pinning the handful of variables we care about keeps
        // diagrams readable on the app's light surfaces everywhere.
        m.default.initialize({
          startOnLoad: false,
          securityLevel: 'strict',
          theme: 'base',
          themeVariables: {
            primaryColor: '#eef2ff',
            primaryTextColor: '#1f2430',
            primaryBorderColor: '#8593c9',
            lineColor: '#5b6472',
            secondaryColor: '#f5f7fa',
            tertiaryColor: '#ffffff',
            background: '#ffffff',
            fontFamily: 'inherit',
          },
        })
        return m.default
      })
    }
    mermaidPromise
      .then(async (mermaid) => {
        if (cancelled) return
        // Mermaid sizes each label's box from a hidden measurement of the
        // text, in the app's font. Measure that before the webfont lands and
        // every box comes out a hair too narrow — the first node rendered as
        // "Draf". The labels live in <foreignObject>, which clips at its own
        // geometry, so no CSS can rescue a box that came out short.
        await document.fonts?.ready
        if (cancelled) return
        const id = `mermaid-${idSeq++}`
        const { svg } = await mermaid.render(id, code)
        if (!cancelled && ref.current) {
          ref.current.innerHTML = svg
          // Mermaid caps the diagram at its natural width from a <style>
          // block inside the SVG — the one this bundle strips. Without the
          // cap its `width: 100%` stretches a three-node flowchart across a
          // full report column, scaling the text (and the clip on every
          // label box) with it. Re-apply the cap from the viewBox.
          const el = ref.current.querySelector('svg')
          const natural = el?.viewBox?.baseVal?.width
          if (el && natural) el.style.maxWidth = `${natural}px`
          setError(null)
        }
      })
      .catch((e) => {
        if (!cancelled) setError(String(e?.message ?? e))
      })
    return () => {
      cancelled = true
    }
  }, [code])

  if (error) {
    return (
      <pre className="md-mermaid-error" data-testid="mermaid-error">
        <code>{code}</code>
      </pre>
    )
  }
  return <div className="md-mermaid" data-testid="mermaid-diagram" ref={ref} />
}
