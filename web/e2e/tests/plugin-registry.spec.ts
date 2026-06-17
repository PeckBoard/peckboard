import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * UI e2e for the Plugin Registry page (Settings → Plugins → "Browse
 * plugins"): two tabs (Plugins / Repositories), search, install, and
 * repository add/remove. Driven against mocked Peckboard endpoints (the
 * stub convention used by the other plugin specs); backend
 * resolve/aggregate/install logic is covered by Rust tests.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'
const REPO_URL = 'https://raw.githubusercontent.com/PeckBoard/plugins/main/registry.json'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

/** Stand up the mocked registry endpoints with mutable repo/install state. */
async function mockRegistry(page: Page) {
  const state = {
    installed: false,
    repos: [{ url: REPO_URL, label: 'PeckBoard/plugins', removable: true, ok: true }] as Array<{
      url: string
      label: string
      removable: boolean
      ok: boolean
    }>,
    lastInstall: null as { id?: string; repository?: string } | null,
    lastAdd: null as { repository?: string } | null,
    lastRemove: null as { url?: string } | null,
  }

  await page.route('**/api/plugins', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ plugins: [], ui_panels: [], wasm_plugins: [] }),
    })
  })

  await page.route('**/api/plugins/registry', async (route) => {
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({
        repositories: state.repos,
        plugins: [
          {
            id: 'demo',
            name: 'Demo Plugin',
            description: 'A demo plugin from the registry.',
            author: 'PeckBoard',
            version: '1.0.0',
            hooks: ['http.request.before'],
            repository: REPO_URL,
            repository_label: 'PeckBoard/plugins',
            installed: state.installed,
          },
        ],
      }),
    })
  })

  await page.route('**/api/plugins/registry/install', async (route) => {
    state.lastInstall = route.request().postDataJSON()
    state.installed = true
    await route.fulfill({
      contentType: 'application/json',
      body: JSON.stringify({ plugin: { name: 'demo', hooks: [], status: 'pending' } }),
    })
  })

  await page.route('**/api/plugins/repositories', async (route) => {
    const method = route.request().method()
    if (method === 'POST') {
      const body = route.request().postDataJSON() as { repository?: string }
      state.lastAdd = body
      const url = `https://raw.githubusercontent.com/${body.repository}/main/registry.json`
      state.repos.push({ url, label: body.repository ?? '', removable: true, ok: true })
      await route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({ repository: { url, label: body.repository, removable: true } }),
      })
    } else if (method === 'DELETE') {
      const body = route.request().postDataJSON() as { url?: string }
      state.lastRemove = body
      state.repos = state.repos.filter((r) => r.url !== body.url)
      await route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({ removed: body.url }),
      })
    } else {
      await route.fulfill({
        contentType: 'application/json',
        body: JSON.stringify({ repositories: state.repos }),
      })
    }
  })

  return state
}

test('browse → search → install from the registry page', async ({ request, page, baseURL }) => {
  expect(baseURL).toBeTruthy()
  const token = await authenticate(request)
  const state = await mockRegistry(page)

  await loadAppAt(page, token, '/plugins')
  await expect(page.getByTestId('plugins-modal')).toBeVisible({ timeout: 10_000 })

  // Available plugins is its OWN page now — not duplicated in the modal.
  await expect(page.getByTestId('registry-plugin-demo')).toHaveCount(0)

  await page.getByTestId('browse-plugins').click()
  await expect(page.getByTestId('plugin-registry-modal')).toBeVisible()

  // Plugins tab is default; the entry is listed.
  const row = page.getByTestId('registry-plugin-demo')
  await expect(row).toBeVisible()
  await expect(row).toContainText('Demo Plugin')

  // Search filters.
  await page.getByTestId('registry-search').fill('zzz-no-match')
  await expect(page.getByTestId('registry-plugin-demo')).toHaveCount(0)
  await page.getByTestId('registry-search').fill('demo')
  await expect(page.getByTestId('registry-plugin-demo')).toBeVisible()

  // Install posts id + source repository, then flips to Installed.
  await page.getByTestId('registry-install-demo').click()
  await expect(page.getByTestId('registry-install-demo')).toHaveText('Installed')
  await expect(page.getByTestId('registry-install-demo')).toBeDisabled()
  expect(state.lastInstall?.id).toBe('demo')
  expect(state.lastInstall?.repository).toBe(REPO_URL)
})

test('back to plugins returns to the Plugins modal', async ({ request, page, baseURL }) => {
  expect(baseURL).toBeTruthy()
  const token = await authenticate(request)
  await mockRegistry(page)

  await loadAppAt(page, token, '/plugins')
  await expect(page.getByTestId('plugins-modal')).toBeVisible({ timeout: 10_000 })

  // Open the registry page from the Plugins modal.
  await page.getByTestId('browse-plugins').click()
  await expect(page.getByTestId('plugin-registry-modal')).toBeVisible()
  await expect(page.getByTestId('plugins-modal')).toHaveCount(0)

  // "Back to plugins" closes the registry and re-opens the Plugins modal.
  await page.getByTestId('registry-back-to-plugins').click()
  await expect(page.getByTestId('plugin-registry-modal')).toHaveCount(0)
  await expect(page.getByTestId('plugins-modal')).toBeVisible()
})

test('repositories tab adds and removes a repository', async ({ request, page, baseURL }) => {
  expect(baseURL).toBeTruthy()
  const token = await authenticate(request)
  const state = await mockRegistry(page)

  await loadAppAt(page, token, '/plugin-registry')
  await expect(page.getByTestId('plugin-registry-modal')).toBeVisible({ timeout: 10_000 })

  await page.getByTestId('registry-tab-repositories').click()
  // Seeded default repo is present.
  await expect(page.getByTestId('registry-repo-0')).toContainText('PeckBoard/plugins')

  // Add a repo by slug.
  await page.getByTestId('registry-repo-input').fill('octo/cat')
  await page.getByTestId('registry-repo-add').click()
  await expect(page.getByTestId('registry-repo-1')).toContainText('octo/cat')
  expect(state.lastAdd?.repository).toBe('octo/cat')

  // Remove it.
  await page.getByTestId('registry-repo-remove-1').click()
  await expect(page.getByTestId('registry-repo-1')).toHaveCount(0)
  expect(state.lastRemove?.url).toContain('octo/cat')
})
