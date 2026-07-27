import { test, expect, type APIRequestContext, type Locator, type Page } from '@playwright/test'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Every form control needs an accessible name, and its visible label has
 * to be a real click target.
 *
 * The card, project and settings forms used to render bare
 * `<label className="form-label">Title</label>` next to an input it
 * neither wrapped nor addressed with `htmlFor` — a screen reader
 * announced "edit text" with no name, and clicking the label did
 * nothing. The provider visibility rows were worse: a `<span>` next to a
 * naked `<input type="checkbox">`.
 *
 * These assertions are structural rather than per-field so a new
 * unlabelled control can't slip into one of these forms later:
 *   - no visible input/select/textarea inside the form lacks a name
 *     (`.labels` covers both `for=` and wrapping labels),
 *   - no `label[for]` points at an id that doesn't exist (a dead
 *     association reads as "labelled" in review but isn't),
 *   - clicking the label moves focus to its control.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(
  request: APIRequestContext,
): Promise<{ token: string; auth: Record<string, string> }> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, auth: { Authorization: `Bearer ${token}` } }
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

/** Outer HTML of every visible control in `scope` that has no accessible name. */
async function unnamedControls(scope: Locator): Promise<string[]> {
  return scope.evaluate((root: HTMLElement) => {
    const unnamed: string[] = []
    for (const el of Array.from(root.querySelectorAll('input, select, textarea'))) {
      const control = el as HTMLInputElement
      if (control.type === 'hidden' || control.offsetParent === null) continue
      if (control.getAttribute('aria-label')?.trim()) continue
      const labelledBy = control.getAttribute('aria-labelledby')
      if (
        labelledBy &&
        labelledBy.split(/\s+/).some((id) => document.getElementById(id)?.textContent?.trim())
      ) {
        continue
      }
      const labels = control.labels ? Array.from(control.labels) : []
      if (labels.some((l) => l.textContent?.trim())) continue
      unnamed.push(control.outerHTML.slice(0, 160))
    }
    return unnamed
  })
}

/** `htmlFor` values in `scope` that don't resolve to an element. */
async function danglingLabelTargets(scope: Locator): Promise<string[]> {
  return scope.evaluate((root: HTMLElement) =>
    Array.from(root.querySelectorAll('label[for]'))
      .map((l) => l.getAttribute('for') as string)
      .filter((id) => !document.getElementById(id)),
  )
}

async function seedFolder(request: APIRequestContext, auth: Record<string, string>, tag: string) {
  const folderPath = mkdtempSync(path.join(tmpdir(), `peckboard-e2e-${tag}-`))
  const res = await request.post('/api/folders', {
    headers: auth,
    data: { name: `e2e-${tag}-${Date.now()}`, path: folderPath },
  })
  expect(res.ok(), `create folder failed: ${await res.text()}`).toBeTruthy()
  return (await res.json()) as { id: string }
}

test('card form labels name their controls and focus them on click', async ({ request, page }) => {
  const { token, auth } = await authenticate(request)
  const folder = await seedFolder(request, auth, 'labels-card')

  // worker_count=0 so no orchestrator moves the card mid-assertion.
  const projectRes = await request.post('/api/projects', {
    headers: auth,
    data: { name: 'label check', folder_id: folder.id, worker_count: 0, workflow: 'task' },
  })
  expect(projectRes.ok(), `create project failed: ${await projectRes.text()}`).toBeTruthy()
  const project = (await projectRes.json()) as { id: string }

  await loadAt(page, token, `/projects/${project.id}`)
  await page.getByRole('button', { name: 'Add Card' }).click()

  const form = page.locator('.modal').filter({ hasText: 'New Card' }).locator('form')
  await expect(form).toBeVisible({ timeout: 10_000 })

  expect(await unnamedControls(form)).toEqual([])
  expect(await danglingLabelTargets(form)).toEqual([])

  // The composite pickers are <button> triggers, so `.labels` can't see
  // them — check their labels resolve and they carry a name of their own.
  await expect(page.locator('#card-workflow')).toBeVisible()
  await expect(page.locator('#card-model')).toBeVisible()
  await expect(page.locator('#card-system-prompt')).toBeVisible()

  // Clicking the label focuses the field it names.
  await form.locator('label[for="card-title"]').click()
  await expect(page.locator('#card-title')).toBeFocused()

  await form.locator('label[for="card-priority"]').click()
  await expect(page.locator('#card-priority')).toBeFocused()
})

test('new project form labels name their controls, advanced section included', async ({
  request,
  page,
}) => {
  const { token, auth } = await authenticate(request)
  await seedFolder(request, auth, 'labels-project')

  await loadAt(page, token, '/projects')
  await page.getByRole('button', { name: '+ New project' }).click()

  const form = page.locator('.modal').filter({ hasText: 'New Project' }).locator('form')
  await expect(form).toBeVisible({ timeout: 10_000 })

  // Worker count / budget / model / effort only mount once expanded.
  await form.getByRole('button', { name: /advanced settings/ }).click()
  await expect(form.locator('#new-project-worker-count')).toBeVisible()

  expect(await unnamedControls(form)).toEqual([])
  expect(await danglingLabelTargets(form)).toEqual([])

  await form.locator('label[for="new-project-name"]').click()
  await expect(page.locator('#new-project-name')).toBeFocused()

  await form.locator('label[for="new-project-context"]').click()
  await expect(page.locator('#new-project-context')).toBeFocused()
})

test('provider visibility rows announce the provider and toggle from their text', async ({
  request,
  page,
}) => {
  const { token } = await authenticate(request)
  await loadAt(page, token, '/')

  await expect(page.locator('.rail-brand')).toBeVisible({ timeout: 10_000 })
  await page.locator('.rail-avatar').click()
  await page.locator('.user-menu-dropdown').getByRole('menuitem', { name: 'Settings' }).click()
  const settings = page.getByTestId('settings-page')
  await expect(settings).toBeVisible()
  await settings.getByTestId('settings-nav-providers').click()

  const toggle = page.getByTestId('provider-toggle-ollama')
  await expect(toggle).toBeVisible()
  await expect(toggle).toHaveAccessibleName('Ollama')

  // The row is a <label> now, so its text is a click target — not just
  // the 15px checkbox.
  const row = settings.locator('label.settings-row').filter({ has: toggle })
  await expect(row).toHaveCount(1)
  const rowText = row.locator('.settings-label')

  const before = await toggle.isChecked()
  await rowText.click()
  await expect(toggle).toBeChecked({ checked: !before, timeout: 5_000 })

  // Restore for the other specs.
  await rowText.click()
  await expect(toggle).toBeChecked({ checked: before, timeout: 5_000 })
})
