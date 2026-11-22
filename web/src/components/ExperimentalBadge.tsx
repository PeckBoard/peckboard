import { EXPERIMENTAL_PLUGIN_TITLE, type PluginStability } from '../utils/pluginApproval'

/**
 * "Experimental" pill for a plugin whose manifest or registry entry declares
 * `stability: "experimental"`. Renders nothing for stable (or unset) plugins,
 * so callers can drop it next to a plugin name unconditionally.
 */
export default function ExperimentalBadge({
  stability,
  testId,
}: {
  stability?: PluginStability | null
  testId?: string
}) {
  if (stability !== 'experimental') return null
  return (
    <span
      className="plugin-badge plugin-badge--experimental"
      title={EXPERIMENTAL_PLUGIN_TITLE}
      data-testid={testId}
    >
      Experimental
    </span>
  )
}
