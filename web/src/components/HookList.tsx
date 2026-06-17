import { hookMeta } from '../utils/pluginApproval'

/**
 * The single, canonical way a plugin's requested hooks are displayed:
 * one row per hook with a bold human-readable title over a muted
 * description, matching the permission rows on built-in plugin cards.
 *
 * The installed-plugins panel, the registry browser, and the startup
 * approval prompt all render hooks through here so the presentations can
 * never drift apart. The raw hook id is kept on each row's `data-hook`
 * for tooling/tests even though it isn't shown. Renders nothing when a
 * plugin declares no hooks. Pass `testId` to tag the list for tests, and
 * `title` (e.g. "Hooks") for a section header matching the Permissions one.
 */
export default function HookList({
  hooks,
  testId,
  title,
}: {
  hooks: string[]
  testId?: string
  title?: string
}) {
  if (hooks.length === 0) return null
  return (
    <div className="plugin-hooks">
      {title && <div className="plugin-section-title">{title}</div>}
      <ul className="plugin-hook-list" data-testid={testId}>
        {hooks.map((h) => {
          const { title: label, description } = hookMeta(h)
          return (
            <li key={h} className="plugin-hook" data-hook={h}>
              <span className="plugin-hook-label">{label}</span>
              <span className="plugin-hook-desc">{description}</span>
            </li>
          )
        })}
      </ul>
    </div>
  )
}
