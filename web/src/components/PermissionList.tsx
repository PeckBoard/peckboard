import { permissionMeta } from '../utils/pluginApproval'

/**
 * The canonical way a WASM plugin's requested host permissions are displayed:
 * one row per permission with a bold human-readable title over a muted
 * description, identical to the {@link HookList} presentation and the
 * built-in plugin cards. Used by the installed-plugins panel and the startup
 * approval prompt so the two never drift. The raw permission id is kept on
 * each row's `data-permission` for tooling/tests. Renders nothing when the
 * plugin requests no permissions. Pass `title` (e.g. "Permissions") for a
 * section header.
 */
export default function PermissionList({
  permissions,
  testId,
  title,
}: {
  permissions: string[]
  testId?: string
  title?: string
}) {
  if (!permissions || permissions.length === 0) return null
  return (
    <div className="plugin-hooks">
      {title && <div className="plugin-section-title">{title}</div>}
      <ul className="plugin-hook-list" data-testid={testId}>
        {permissions.map((p) => {
          const { title: label, description } = permissionMeta(p)
          return (
            <li key={p} className="plugin-hook" data-permission={p}>
              <span className="plugin-hook-label">{label}</span>
              <span className="plugin-hook-desc">{description}</span>
            </li>
          )
        })}
      </ul>
    </div>
  )
}
