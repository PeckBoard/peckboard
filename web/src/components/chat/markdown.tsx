import type { Components } from 'react-markdown'

import MermaidBlock from '../MermaidBlock'
import { isMermaidCode, mermaidSource } from '../mermaidFence'

/** The component overrides every chat-shaped markdown surface shares: fenced
 *  ```mermaid blocks render as diagrams, everything else falls through to
 *  react-markdown's defaults (and stays sanitised — see [[SafeMarkdown]]).
 *
 *  Lives outside ChatView so the review screen's chat lane renders assistant
 *  replies exactly the way the main chat does, rather than growing a second
 *  copy that drifts. */
export const chatMarkdownComponents: Components = {
  code({ node, className, children }) {
    // The source comes off the parsed node, not `children`: these surfaces
    // also run rehype-highlight, and a highlighted fence's children are
    // token spans rather than the one text node `String(children)` needs.
    if (isMermaidCode(node)) return <MermaidBlock code={mermaidSource(node)} />
    return <code className={className}>{children}</code>
  },
}
