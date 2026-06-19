import { useEffect, useMemo, useState } from 'react'
import { authedFetch } from '../store/auth'

/**
 * Renders the inputs for a single built-in plugin's settings, driven by
 * the typed schema the backend returns in `/api/plugins`. Mirrors the
 * Rust `FieldKind` enum — every variant added there has to grow a
 * matching renderer here.
 *
 * Secret values (string `secret: true`, key-value `secret_values: true`)
 * are never echoed back from the server, so the inputs render empty with
 * a "currently set" hint when `has_value` is true. The user has to
 * re-enter a value to change it; leaving it blank preserves the stored
 * value rather than clearing it (clearing is an explicit "remove" action
 * for KV rows).
 */

type FieldKind =
  | { type: 'string'; secret?: boolean; default?: string; placeholder?: string }
  | { type: 'url'; default?: string; placeholder?: string }
  | { type: 'integer'; default?: number; min?: number; max?: number }
  | { type: 'boolean'; default?: boolean }
  | {
      type: 'enum'
      options: { value: string; label: string }[]
      default?: string
    }
  | {
      type: 'key_value_list'
      secret_values?: boolean
      key_placeholder?: string
      value_placeholder?: string
    }
  | { type: 'string_list'; item_placeholder?: string }

interface SchemaField {
  key: string
  title: string
  description?: string | null
  required?: boolean
  type: FieldKind['type']
  secret?: boolean
  default?: unknown
  placeholder?: string
  min?: number
  max?: number
  options?: { value: string; label: string }[]
  secret_values?: boolean
  key_placeholder?: string
  value_placeholder?: string
  item_placeholder?: string
}

interface Schema {
  fields: SchemaField[]
}

interface StoredField {
  key: string
  value: unknown
  has_value: boolean
  masked: boolean
}

interface SettingsPayload {
  plugin_id: string
  schema: Schema
  settings: StoredField[]
}

interface KvPair {
  key: string
  value: string
}

type FormValue = string | number | boolean | KvPair[] | string[]

interface FormState {
  values: Record<string, FormValue>
  dirty: Record<string, boolean>
}

function initialValue(field: SchemaField, stored: StoredField | undefined): FormValue {
  switch (field.type) {
    case 'string':
    case 'url':
    case 'enum':
      if (stored && !stored.masked && typeof stored.value === 'string') return stored.value
      // Secret strings: the API never returns the value; start empty
      // so the user can either leave it (preserves stored value) or
      // type a replacement.
      return ''
    case 'integer':
      if (stored && typeof stored.value === 'number') return stored.value
      return typeof field.default === 'number' ? field.default : 0
    case 'boolean':
      if (stored && typeof stored.value === 'boolean') return stored.value
      return Boolean(field.default)
    case 'key_value_list':
      if (stored && Array.isArray(stored.value)) {
        // Stored entries arrive in the redacted shape: keys present,
        // values null when masked. We seed the form with the keys so
        // the user can see what's stored; values stay empty until they
        // re-enter.
        return (stored.value as { key: string; value: unknown }[]).map((entry) => ({
          key: typeof entry.key === 'string' ? entry.key : '',
          value: typeof entry.value === 'string' ? entry.value : '',
        }))
      }
      return []
    case 'string_list':
      if (stored && Array.isArray(stored.value)) {
        return (stored.value as unknown[]).filter((v): v is string => typeof v === 'string')
      }
      return []
  }
}

function emptyKvPair(): KvPair {
  return { key: '', value: '' }
}

function seedForm(payload: SettingsPayload): FormState {
  const storedByKey = new Map<string, StoredField>(payload.settings.map((s) => [s.key, s]))
  const values: Record<string, FormValue> = {}
  for (const field of payload.schema.fields) {
    values[field.key] = initialValue(field, storedByKey.get(field.key))
  }
  return { values, dirty: {} }
}

export default function PluginSettingsForm({ pluginId }: { pluginId: string }) {
  const [payload, setPayload] = useState<SettingsPayload | null>(null)
  const [form, setForm] = useState<FormState>({ values: {}, dirty: {} })
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)

  // Both the initial load and a successful save call `applyPayload`,
  // which seeds the form alongside storing the new payload. That keeps
  // the form-seed logic out of an effect (the eslint react-hooks plugin
  // forbids setState() in an effect body) without losing the
  // "successful save → refresh masked view" behaviour.
  const applyPayload = (next: SettingsPayload) => {
    setPayload(next)
    setForm(seedForm(next))
  }

  useEffect(() => {
    let cancelled = false
    authedFetch(`/api/plugins/${encodeURIComponent(pluginId)}/settings`)
      .then((res) => (res.ok ? res.json() : Promise.reject(new Error(`HTTP ${res.status}`))))
      .then((data: SettingsPayload) => {
        if (!cancelled) applyPayload(data)
      })
      .catch((e: Error) => {
        if (!cancelled) setError(e.message)
      })
    return () => {
      cancelled = true
    }
  }, [pluginId])

  const storedByKey = useMemo(() => {
    const map = new Map<string, StoredField>()
    payload?.settings.forEach((s) => map.set(s.key, s))
    return map
  }, [payload])

  if (error && !payload) {
    return <p className="plugin-settings-error">Failed to load settings: {error}</p>
  }
  if (!payload) {
    return <p className="settings-loading">Loading settings…</p>
  }
  if (payload.schema.fields.length === 0) {
    return null
  }

  const update = (key: string, value: FormValue) => {
    setSaved(false)
    setForm((prev) => ({
      values: { ...prev.values, [key]: value },
      dirty: { ...prev.dirty, [key]: true },
    }))
  }

  const handleSave = async () => {
    setSaving(true)
    setError(null)
    setSaved(false)
    // Only send fields the user actually touched. For secret fields,
    // this preserves the stored value when the input is left untouched
    // (typing into a secret string field marks it dirty and replaces
    // the stored value).
    const updates: Record<string, unknown> = {}
    for (const field of payload.schema.fields) {
      if (!form.dirty[field.key]) continue
      const raw = form.values[field.key]
      switch (field.type) {
        case 'string':
        case 'url':
        case 'enum':
          updates[field.key] = typeof raw === 'string' ? raw : ''
          break
        case 'integer':
          updates[field.key] = typeof raw === 'number' ? raw : Number(raw) || 0
          break
        case 'boolean':
          updates[field.key] = Boolean(raw)
          break
        case 'key_value_list':
          updates[field.key] = (Array.isArray(raw) ? (raw as KvPair[]) : [])
            .filter((p) => p.key.trim() !== '')
            .map((p) => ({ key: p.key.trim(), value: p.value }))
          break
        case 'string_list':
          updates[field.key] = (Array.isArray(raw) ? (raw as string[]) : [])
            .map((s) => s.trim())
            .filter((s) => s !== '')
          break
      }
    }

    try {
      const res = await authedFetch(`/api/plugins/${encodeURIComponent(pluginId)}/settings`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ updates }),
      })
      const data = await res.json().catch(() => ({}))
      if (!res.ok) {
        const message = (typeof data?.error === 'string' && data.error) || `HTTP ${res.status}`
        const field = typeof data?.field === 'string' ? data.field : ''
        setError(field ? `${field}: ${message}` : message)
        return
      }
      applyPayload(data as SettingsPayload)
      setSaved(true)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Network error')
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="plugin-settings" data-testid={`plugin-settings-${pluginId}`}>
      {payload.schema.fields.map((field) => (
        <FieldRow
          key={field.key}
          field={field}
          value={form.values[field.key]}
          stored={storedByKey.get(field.key)}
          dirty={Boolean(form.dirty[field.key])}
          onChange={(v) => update(field.key, v)}
        />
      ))}
      <div className="plugin-settings-actions">
        <button
          type="button"
          className="plugin-settings-save"
          onClick={handleSave}
          disabled={saving || Object.keys(form.dirty).length === 0}
        >
          {saving ? 'Saving…' : 'Save settings'}
        </button>
        {error && <span className="plugin-settings-error">{error}</span>}
        {saved && !error && <span className="plugin-settings-success">Saved.</span>}
      </div>
    </div>
  )
}

function FieldRow({
  field,
  value,
  stored,
  dirty,
  onChange,
}: {
  field: SchemaField
  value: FormValue
  stored: StoredField | undefined
  dirty: boolean
  onChange: (v: FormValue) => void
}) {
  const labelNode = (
    <span className="plugin-setting-label">
      {field.title}
      {field.required && (
        <span className="plugin-setting-required" aria-hidden>
          *
        </span>
      )}
    </span>
  )
  const descNode = field.description ? (
    <span className="plugin-setting-desc">{field.description}</span>
  ) : null
  const secretHint =
    stored?.has_value && stored.masked && !dirty ? (
      <span className="plugin-setting-secret-set">A value is currently saved.</span>
    ) : null

  switch (field.type) {
    case 'string':
    case 'url': {
      const placeholder = field.placeholder ?? ''
      return (
        <label className="plugin-setting-field" data-field={field.key}>
          {labelNode}
          {descNode}
          <input
            className="plugin-setting-input"
            type={field.type === 'url' ? 'url' : field.secret ? 'password' : 'text'}
            value={typeof value === 'string' ? value : ''}
            placeholder={placeholder}
            onChange={(e) => onChange(e.target.value)}
          />
          {secretHint}
        </label>
      )
    }
    case 'integer': {
      return (
        <label className="plugin-setting-field" data-field={field.key}>
          {labelNode}
          {descNode}
          <input
            className="plugin-setting-input"
            type="number"
            min={field.min}
            max={field.max}
            value={typeof value === 'number' ? value : Number(value) || 0}
            onChange={(e) => onChange(e.target.value === '' ? 0 : Number(e.target.value))}
          />
        </label>
      )
    }
    case 'boolean': {
      return (
        <label className="plugin-setting-field plugin-setting-checkbox" data-field={field.key}>
          <input
            type="checkbox"
            checked={Boolean(value)}
            onChange={(e) => onChange(e.target.checked)}
          />
          {labelNode}
          {descNode}
        </label>
      )
    }
    case 'enum': {
      const options = field.options ?? []
      return (
        <label className="plugin-setting-field" data-field={field.key}>
          {labelNode}
          {descNode}
          <select
            className="plugin-setting-select"
            value={typeof value === 'string' ? value : ''}
            onChange={(e) => onChange(e.target.value)}
          >
            {options.map((opt) => (
              <option key={opt.value} value={opt.value}>
                {opt.label}
              </option>
            ))}
          </select>
        </label>
      )
    }
    case 'key_value_list': {
      const pairs = Array.isArray(value) ? (value as KvPair[]) : []
      return (
        <div className="plugin-setting-field" data-field={field.key}>
          {labelNode}
          {descNode}
          {pairs.map((pair, i) => (
            <div key={i} className="plugin-setting-kv-row">
              <input
                type="text"
                placeholder={field.key_placeholder ?? 'Key'}
                value={pair.key}
                onChange={(e) => {
                  const next = [...pairs]
                  next[i] = { ...next[i], key: e.target.value }
                  onChange(next)
                }}
              />
              <input
                type={field.secret_values ? 'password' : 'text'}
                placeholder={field.value_placeholder ?? 'Value'}
                value={pair.value}
                onChange={(e) => {
                  const next = [...pairs]
                  next[i] = { ...next[i], value: e.target.value }
                  onChange(next)
                }}
              />
              <button
                type="button"
                className="plugin-setting-kv-remove"
                onClick={() => {
                  const next = pairs.filter((_, idx) => idx !== i)
                  onChange(next)
                }}
              >
                Remove
              </button>
            </div>
          ))}
          <button
            type="button"
            className="plugin-setting-kv-add"
            onClick={() => onChange([...pairs, emptyKvPair()])}
          >
            + Add header
          </button>
          {secretHint}
        </div>
      )
    }
    case 'string_list': {
      const items = Array.isArray(value) ? (value as string[]) : []
      return (
        <div className="plugin-setting-field" data-field={field.key}>
          {labelNode}
          {descNode}
          {items.map((item, i) => (
            <div key={i} className="plugin-setting-kv-row">
              <input
                type="text"
                placeholder={field.item_placeholder ?? 'Value'}
                value={item}
                onChange={(e) => {
                  const next = [...items]
                  next[i] = e.target.value
                  onChange(next)
                }}
              />
              <button
                type="button"
                className="plugin-setting-kv-remove"
                onClick={() => onChange(items.filter((_, idx) => idx !== i))}
              >
                Remove
              </button>
            </div>
          ))}
          <button
            type="button"
            className="plugin-setting-kv-add"
            onClick={() => onChange([...items, ''])}
          >
            + Add model
          </button>
        </div>
      )
    }
  }
}
