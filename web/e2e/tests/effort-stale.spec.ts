import { test, expect } from '@playwright/test'
import { effortSelectOptions, effortOptionsForModel, sessionModelPatch } from '../../src/lib/effort'

/**
 * A session's effort must not outlive its provider: switching e.g.
 * claude(effort high) → cursor (no effort ladder) has to clear the effort in
 * the same PATCH, and a form holding a stored-but-invalid effort must render
 * it as an explicit "(unavailable)" row instead of a blank select that
 * silently round-trips the stale value.
 *
 * These exercise the pure helpers in `src/lib/effort.ts` directly (node-side
 * — no page). ChatView builds every model-change PATCH through
 * `sessionModelPatch`, and the card/task/project forms build their `<select>`
 * options through `effortSelectOptions`, so the semantics proven here are the
 * ones the UI ships.
 */

const LADDER = [
  { id: 'low', label: 'Low' },
  { id: 'medium', label: 'Medium' },
  { id: 'high', label: 'High' },
  { id: 'xhigh', label: 'Extra high' },
  { id: 'max', label: 'Max' },
]

const PROVIDERS = [
  { id: 'claude', effort_levels: LADDER },
  { id: 'cursor', effort_levels: [] },
]

test.describe('sessionModelPatch (chat model switch)', () => {
  test('clears effort when the target provider has no effort ladder', () => {
    expect(sessionModelPatch('cursor:gpt-5', 'high', PROVIDERS)).toEqual({
      model: 'cursor:gpt-5',
      effort: null,
    })
  })

  test('keeps effort when the target provider offers the level', () => {
    expect(sessionModelPatch('claude:opus', 'high', PROVIDERS)).toEqual({
      model: 'claude:opus',
    })
  })

  test('same-provider model change never clears', () => {
    expect(sessionModelPatch('claude:sonnet', 'max', PROVIDERS)).toEqual({
      model: 'claude:sonnet',
    })
  })

  test('no stored effort → plain model patch', () => {
    expect(sessionModelPatch('cursor:gpt-5', null, PROVIDERS)).toEqual({ model: 'cursor:gpt-5' })
    expect(sessionModelPatch('cursor:gpt-5', '', PROVIDERS)).toEqual({ model: 'cursor:gpt-5' })
  })

  test('unloaded catalogue (providers=[]) never clears', () => {
    // Mirrors the forms' clear-guard: a transient /api/models failure must
    // not wipe a stored effort.
    expect(sessionModelPatch('cursor:gpt-5', 'high', [])).toEqual({ model: 'cursor:gpt-5' })
  })

  test('unknown target provider clears a stored effort', () => {
    expect(sessionModelPatch('ghost:model', 'high', PROVIDERS)).toEqual({
      model: 'ghost:model',
      effort: null,
    })
  })

  test('bare model id resolves to claude and keeps a ladder effort', () => {
    expect(sessionModelPatch('claude-opus-4-8', 'xhigh', PROVIDERS)).toEqual({
      model: 'claude-opus-4-8',
    })
  })
})

test.describe('effortSelectOptions (form load normalization)', () => {
  test('stored effort not offered → explicit "(unavailable)" row appended', () => {
    const opts = effortSelectOptions(effortOptionsForModel('cursor:gpt-5', PROVIDERS), 'high')
    expect(opts).toEqual([
      { value: '', label: 'Default' },
      { value: 'high', label: 'high (unavailable)' },
    ])
  })

  test('catalogue fetch failed (providers=[]) → stale value still visible', () => {
    const opts = effortSelectOptions(effortOptionsForModel('claude:opus', []), 'max')
    expect(opts.map((o) => o.value)).toEqual(['', 'max'])
    expect(opts[1].label).toBe('max (unavailable)')
  })

  test('valid stored effort → options unchanged', () => {
    const base = effortOptionsForModel('claude:opus', PROVIDERS)
    expect(effortSelectOptions(base, 'high')).toEqual(base)
  })

  test('empty effort → options unchanged', () => {
    const base = effortOptionsForModel('cursor:gpt-5', PROVIDERS)
    expect(effortSelectOptions(base, '')).toEqual(base)
  })
})

/**
 * Backend counterpart: `PATCH {effort: null}` must actually clear the stored
 * effort. The update request structs used plain `Option<Option<String>>`,
 * where serde collapses an explicit JSON `null` into "field absent" — so
 * every clear (the chat switch's `sessionModelPatch`, the pickers' Default
 * choice) was silently dropped. Now deserialized via `explicit_null`.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

test.describe('PATCH effort:null clears server-side', () => {
  test('session: model switch + effort:null in one PATCH clears the effort', async ({
    request,
  }) => {
    const login = await request.post('/api/auth/login', {
      data: { username: E2E_USER, password: E2E_PASS },
    })
    expect(login.ok(), `login failed: ${await login.text()}`).toBeTruthy()
    const { token } = (await login.json()) as { token: string }
    const authHeader = { Authorization: `Bearer ${token}` }

    const { mkdtempSync } = await import('node:fs')
    const { tmpdir } = await import('node:os')
    const path = await import('node:path')
    const folderRes = await request.post('/api/folders', {
      headers: authHeader,
      data: {
        name: `e2e-effort-stale-${Date.now()}`,
        path: mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-effort-stale-')),
      },
    })
    expect(folderRes.ok()).toBeTruthy()
    const folder = (await folderRes.json()) as { id: string }

    const createRes = await request.post('/api/sessions', {
      headers: authHeader,
      data: {
        name: 'effort-stale-session',
        folder_id: folder.id,
        model: 'claude:claude-opus-5',
        effort: 'high',
      },
    })
    expect(createRes.ok(), `session create failed: ${await createRes.text()}`).toBeTruthy()
    const session = (await createRes.json()) as { id: string; effort: string | null }
    expect(session.effort).toBe('high')

    // The exact PATCH `sessionModelPatch` produces for claude(high) → cursor.
    const patchRes = await request.patch(`/api/sessions/${session.id}`, {
      headers: authHeader,
      data: { model: 'cursor:gpt-5', effort: null },
    })
    expect(patchRes.ok(), `patch failed: ${await patchRes.text()}`).toBeTruthy()
    const patched = (await patchRes.json()) as { model: string | null; effort: string | null }
    expect(patched.model).toBe('cursor:gpt-5')
    expect(patched.effort).toBeNull()

    // And an absent key still leaves the field alone.
    const renameRes = await request.patch(`/api/sessions/${session.id}`, {
      headers: authHeader,
      data: { name: 'still-no-effort' },
    })
    expect(renameRes.ok()).toBeTruthy()
    expect(((await renameRes.json()) as { effort: string | null }).effort).toBeNull()

    await request.delete(`/api/sessions/${session.id}`, { headers: authHeader })
  })
})
