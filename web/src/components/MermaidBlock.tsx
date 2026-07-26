import { useEffect, useRef, useState } from 'react'

let mermaidPromise: Promise<typeof import('mermaid').default> | null = null
let idSeq = 0

/** Lazy-load mermaid only when a plan with a ```mermaid block is opened, so
 *  it never weighs down the main bundle. Renders the diagram to SVG; on any
 *  parse/render error it falls back to showing the raw source so the plan is
 *  never blank. */
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
        const id = `mermaid-${idSeq++}`
        const { svg } = await mermaid.render(id, code)
        if (!cancelled && ref.current) {
          ref.current.innerHTML = svg
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
      <pre className="plan-mermaid-error" data-testid="mermaid-error">
        <code>{code}</code>
      </pre>
    )
  }
  return <div className="plan-mermaid" data-testid="mermaid-diagram" ref={ref} />
}
