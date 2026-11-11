import { useCallback, useEffect, useMemo, useState } from 'react'
import { authedFetch } from '../store/auth'
import ConfirmDialog from './ConfirmDialog'
import ModelPicker from './ModelPicker'

interface TierRow {
  tier: number
  label: string
  models: string[]
}

interface CatalogModel {
  /** Base model id, without any `@account` suffix. */
  id: string
  display_name: string
  /** The provider's own tier for this model, before any assignment. */
  default_tier: number
}

/** `GET /api/settings/provider-tiers/{provider}`. */
interface ProviderTiers {
  provider: string
  display_name: string
  tiers: TierRow[]
  catalog: CatalogModel[]
}

interface Props {
  providerId: string
  /** Fired after a save, so the app's model catalogue can refresh. */
  onSaved: () => void
}

async function errorMessage(res: Response): Promise<string> {
  const data = (await res.json().catch(() => null)) as { error?: unknown } | null
  return typeof data?.error === 'string' ? data.error : `HTTP ${res.status}`
}

/**
 * "Model Tiers" editor inside a provider's section on Settings → Providers &
 * Accounts. Each change saves straight away and the server's reply becomes the
 * new state, so a refused save can never stay on screen as if it stuck.
 */
export default function ProviderModelTiers({ providerId, onSaved }: Props) {
  const [data, setData] = useState<ProviderTiers | null>(null)
  const [loadFailed, setLoadFailed] = useState(false)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [confirmReset, setConfirmReset] = useState(false)
  const url = `/api/settings/provider-tiers/${encodeURIComponent(providerId)}`

  useEffect(() => {
    let cancelled = false
    authedFetch(url)
      .then(async (res) => {
        if (!res.ok) throw new Error(await errorMessage(res))
        return (await res.json()) as ProviderTiers
      })
      .then((body) => {
        if (!cancelled) setData(body)
      })
      .catch(() => {
        if (!cancelled) setLoadFailed(true)
      })
    return () => {
      cancelled = true
    }
  }, [url])

  const save = useCallback(
    async (tiers: TierRow[]): Promise<boolean> => {
      setBusy(true)
      setError(null)
      try {
        const res = await authedFetch(url, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            tiers: tiers.map(({ tier, models }) => ({ tier, models })),
          }),
        })
        if (!res.ok) throw new Error(await errorMessage(res))
        setData((await res.json()) as ProviderTiers)
        onSaved()
        return true
      } catch (e) {
        setError(`Could not save model tiers: ${(e as Error).message}`)
        return false
      } finally {
        setBusy(false)
      }
    },
    [url, onSaved],
  )

  const assigned = useMemo(() => new Set(data?.tiers.flatMap((t) => t.models)), [data])
  if (loadFailed) {
    return (
      <div className="provider-model-tiers">
        <h4>Model Tiers</h4>
        <p className="form-error" role="alert">
          Could not load this provider&apos;s model tiers.
        </p>
      </div>
    )
  }
  if (!data) {
    return (
      <div className="provider-model-tiers">
        <h4>Model Tiers</h4>
        <p className="settings-loading">Loading models...</p>
      </div>
    )
  }

  // Nothing to choose from and nothing to manage: four empty rows would only
  // offer pickers with no options.
  if (data.catalog.length === 0 && assigned.size === 0) {
    return (
      <div className="provider-model-tiers" data-testid={`provider-tiers-${providerId}`}>
        <h4>Model Tiers</h4>
        <p className="form-hint">
          This provider has no models right now. Check its accounts and settings.
        </p>
      </div>
    )
  }
  const nameOf = (id: string) => data.catalog.find((m) => m.id === id)?.display_name
  const withModels = (tier: number, models: string[]) =>
    data.tiers.map((row) => (row.tier === tier ? { ...row, models } : row))
  // A model may serve several tiers, so each row offers every model it
  // doesn't already hold.
  const addable = (row: TierRow) =>
    data.catalog
      .filter((m) => !row.models.includes(m.id))
      .map((m) => ({ id: m.id, display_name: m.display_name }))
  const move = (row: TierRow, index: number, by: number) => {
    const models = [...row.models]
    const [id] = models.splice(index, 1)
    models.splice(index + by, 0, id)
    void save(withModels(row.tier, models))
  }

  return (
    <div className="provider-model-tiers" data-testid={`provider-tiers-${providerId}`}>
      <div className="provider-base-prompt-header">
        <h4>Model Tiers</h4>
        {assigned.size > 0 && (
          <span
            className="plugin-badge plugin-badge--pending"
            data-testid={`provider-tiers-customized-${providerId}`}
          >
            Customized
          </span>
        )}
      </div>
      <p className="form-hint">
        Choose the models for each tier. Auto uses the first available model in the tier that
        matches a task&apos;s effort, and fallbacks and cost guidance follow the same tiers.
        Unassigned models keep their default tier. Running sessions keep their current model.
      </p>
      {data.catalog.length === 0 && (
        <p className="form-hint">
          This provider has no models right now. Check its accounts and settings.
        </p>
      )}
      <div className="provider-tier-rows">
        {data.tiers.map((row) => {
          const defaults = data.catalog.filter(
            (m) => m.default_tier === row.tier && !assigned.has(m.id),
          )
          return (
            <div
              className="provider-tier-row"
              key={row.tier}
              data-testid={`provider-tier-${providerId}-${row.tier}`}
            >
              <span className="provider-tier-label">{row.label}</span>
              <div className="provider-tier-body">
                <div className="provider-tier-models">
                  {row.models.map((id, index) => {
                    const name = nameOf(id)
                    return (
                      <span
                        className="provider-tier-chip"
                        key={id}
                        data-testid={`provider-tier-chip-${id}`}
                      >
                        {row.models.length > 1 && (
                          <button
                            type="button"
                            className="provider-tier-chip-btn"
                            disabled={busy || index === 0}
                            aria-label={`Move ${name ?? id} earlier in ${row.label}`}
                            onClick={() => move(row, index, -1)}
                          >
                            ‹
                          </button>
                        )}
                        <span>{name ?? id}</span>
                        {name === undefined && (
                          <span className="provider-tier-chip-missing">unavailable</span>
                        )}
                        {row.models.length > 1 && (
                          <button
                            type="button"
                            className="provider-tier-chip-btn"
                            disabled={busy || index === row.models.length - 1}
                            aria-label={`Move ${name ?? id} later in ${row.label}`}
                            onClick={() => move(row, index, 1)}
                          >
                            ›
                          </button>
                        )}
                        <button
                          type="button"
                          className="provider-tier-chip-btn"
                          disabled={busy}
                          aria-label={`Remove ${name ?? id} from ${row.label}`}
                          onClick={() =>
                            void save(
                              withModels(
                                row.tier,
                                row.models.filter((m) => m !== id),
                              ),
                            )
                          }
                        >
                          ×
                        </button>
                      </span>
                    )
                  })}
                  <div className="provider-tier-add">
                    <ModelPicker
                      value=""
                      valueLabel="Add model…"
                      onChange={(id) => void save(withModels(row.tier, [...row.models, id]))}
                      models={addable(row)}
                      ariaLabel={`Add a model to ${row.label}`}
                      disabled={busy}
                      emptyHint="No models left to add"
                      testId={`provider-tier-add-${providerId}-${row.tier}`}
                    />
                  </div>
                </div>
                {defaults.length > 0 && (
                  <p className="form-hint provider-tier-defaults">
                    Default: {defaults.map((m) => m.display_name).join(', ')}
                  </p>
                )}
              </div>
            </div>
          )
        })}
      </div>
      {error && !confirmReset && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      <div className="form-actions">
        {assigned.size === 0 && (
          <span className="form-actions-reason">Using the provider&apos;s default tiers.</span>
        )}
        <button
          type="button"
          className="btn-secondary"
          disabled={busy || assigned.size === 0}
          onClick={() => {
            setError(null)
            setConfirmReset(true)
          }}
          data-testid={`provider-tiers-reset-${providerId}`}
        >
          Reset to defaults
        </button>
      </div>
      {confirmReset && (
        <ConfirmDialog
          title="Reset model tiers?"
          message={`Discard your ${data.display_name} tier choices and use the provider's default tiers.`}
          confirmLabel="Reset to defaults"
          danger
          busy={busy}
          busyLabel="Resetting…"
          error={error}
          onConfirm={async () => {
            if (await save(data.tiers.map((row) => ({ ...row, models: [] })))) {
              setConfirmReset(false)
            }
          }}
          onCancel={() => setConfirmReset(false)}
        />
      )}
    </div>
  )
}
