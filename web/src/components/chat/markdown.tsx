import type { Components } from 'react-markdown'

import MermaidBlock from '../MermaidBlock'

/** The component overrides every chat-shaped markdown surface shares: fenced
 *  ```mermaid blocks render as diagrams, everything else falls through to
 *  react-markdown's defaults (and stays sanitised — see [[SafeMarkdown]]).
 *
 *  Lives outside ChatView so the review screen's chat lane renders assistant
 *  replies exactly the way the main chat does, rather than growing a second
 *  copy that drifts. */
export const chatMarkdownComponents: Components = {
  code({ className, children }) {
    const text = String(children ?? '')
    if (className && /\blanguage-mermaid\b/.test(className)) {
      return <MermaidBlock code={text.replace(/\n$/, '')} />
    }
    return <code className={className}>{children}</code>
  },
}
