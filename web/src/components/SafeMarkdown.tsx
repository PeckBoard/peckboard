import type { ComponentProps } from 'react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'

type RehypePlugins = ComponentProps<typeof ReactMarkdown>['rehypePlugins']

interface SafeMarkdownProps {
  children: string
  className?: string
  /** Extra rehype plugins (e.g. rehype-highlight for code blocks). */
  rehypePlugins?: RehypePlugins
  /** Override or extend the default component map. */
  components?: Components
}

// Render markdown without trusting raw HTML in the source.
//
// react-markdown escapes raw HTML by default (no `rehype-raw` here, and
// none must be added) and applies its built-in `urlTransform` which
// strips `javascript:` / `data:` / `vbscript:` schemes. That gives us
// the same sanitization story the chat view and report viewer relied on,
// in one place — so a future caller can't accidentally render markdown
// from an untrusted source with HTML passthrough enabled.
export default function SafeMarkdown({
  children,
  className,
  rehypePlugins,
  components,
}: SafeMarkdownProps) {
  const mergedComponents: Components = {
    a: ({ href, children: linkChildren }) => (
      <a href={href} target="_blank" rel="noreferrer noopener">
        {linkChildren}
      </a>
    ),
    ...(components ?? {}),
  }
  return (
    <div className={className}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={rehypePlugins}
        components={mergedComponents}
      >
        {children}
      </ReactMarkdown>
    </div>
  )
}
