import type { Components } from 'react-markdown'

import HtmlFence from '../HtmlFence'
import MermaidBlock from '../MermaidBlock'
import { isHtmlCode, isMermaidCode, mermaidSource } from '../mermaidFence'

/** The component overrides every chat-shaped markdown surface shares: fenced
 *  ```mermaid blocks render as diagrams, ```html blocks can open as a live
 *  mockup, everything else falls through to react-markdown's defaults (and
 *  stays sanitised — see [[SafeMarkdown]]).
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
    if (isHtmlCode(node)) {
      return (
        <HtmlFence html={mermaidSource(node)} className={className}>
          {children}
        </HtmlFence>
      )
    }
    return <code className={className}>{children}</code>
  },
}
