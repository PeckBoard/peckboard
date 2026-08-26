import { test, expect, type APIRequestContext } from '@playwright/test'
import { spawnSync } from 'node:child_process'
import { mkdtempSync, mkdirSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Folder repo browser (`/folders/<id>/repos`):
 *
 *  - The Folders page gets a "Repos" button per folder row.
 *  - It opens the repo list: every git repo found in the folder's tree
 *    (recursive scan), with branch + path.
 *  - Clicking a repo opens its own diff viewer: per-file unified diffs of
 *    the working tree vs HEAD (modified + untracked), with counts.
 *  - A clean repo shows the explicit "working tree clean" empty state.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { Authorization: `Bearer ${token}` }
}

function git(cwd: string, args: string[]) {
  const res = spawnSync('git', ['-C', cwd, ...args], { encoding: 'utf8' })
  if (res.status !== 0) {
    throw new Error(`git ${args.join(' ')} failed (${res.status}): ${res.stderr}${res.stdout}`)
  }
}

/** A git repo at `dir` with one commit containing README.md. */
function makeRepo(dir: string) {
  mkdirSync(dir, { recursive: true })
  git(dir, ['init', '-b', 'main'])
  git(dir, ['config', 'user.email', 'e2e@example.com'])
  git(dir, ['config', 'user.name', 'E2E'])
  writeFileSync(path.join(dir, 'README.md'), 'one\ntwo\n')
  git(dir, ['add', '.'])
  git(dir, ['commit', '-m', 'init'])
}

async function loginUi(page: import('@playwright/test').Page, baseURL: string) {
  await page.goto(baseURL)
  await page.getByLabel('Username').fill(E2E_USER)
  await page.getByLabel('Password').fill(E2E_PASS)
  await page.getByRole('button', { name: /sign in/i }).click()
  await expect(page.locator('.rail')).toBeVisible()
}

test('folder page → repo list → per-repo diff viewer', async ({ page, baseURL, request }) => {
  expect(baseURL).toBeTruthy()
  const auth = await authenticate(request)

  // Folder root is itself a repo, with two nested repos in subfolders:
  // one dirty, one clean. All three must show up in the list.
  const root = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-repos-'))
  makeRepo(root)
  const dirty = path.join(root, 'apps', 'web-repo')
  const clean = path.join(root, 'tools', 'cli-repo')
  makeRepo(dirty)
  makeRepo(clean)
  writeFileSync(path.join(dirty, 'README.md'), 'one\nTWO\n') // modified
  writeFileSync(path.join(dirty, 'notes.txt'), 'fresh\n') // untracked

  const folderName = `e2e-repos-${Date.now()}`
  const folderRes = await request.post('/api/folders', {
    headers: auth,
    data: { name: folderName, path: root },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }

  await loginUi(page, baseURL!)
  await page.locator('.rail-btn[title="Folders"]').click()
  await expect(page).toHaveURL(/\/folders$/)

  // Folder row → Repos button → repo list.
  await page.getByTestId(`folder-repos-${folderName}`).click()
  await expect(page).toHaveURL(new RegExp(`/folders/${folder.id}/repos$`))
  await expect(page.getByTestId('repo-list-view')).toBeVisible()
  await expect(page.getByRole('heading', { name: `Repos — ${folderName}` })).toBeVisible()
  await expect(page.locator('.list-view-name', { hasText: 'web-repo' })).toBeVisible()
  await expect(page.locator('.list-view-name', { hasText: 'cli-repo' })).toBeVisible()
  // The folder-root repo is listed under the folder's own name.
  await expect(page.locator('.list-view-name', { hasText: folderName })).toBeVisible()
  await expect(page.locator('.repo-path-tag', { hasText: 'apps/web-repo' })).toBeVisible()

  // Dirty repo → its diff viewer: 2 files, modified + untracked.
  await page.locator('.list-view-name', { hasText: 'web-repo' }).click()
  await expect(page.getByTestId('repo-diff-view')).toBeVisible()
  await expect(page.getByTestId('repo-diff-summary')).toContainText('2 changed files')
  await expect(page.getByText('modified', { exact: true })).toBeVisible()
  await expect(page.getByText('untracked', { exact: true })).toBeVisible()

  // Expanding the README diff shows the actual hunk lines.
  await page.getByRole('button', { name: /README\.md/ }).click()
  await expect(page.locator('.diff-line-add', { hasText: '+TWO' })).toBeVisible()
  await expect(page.locator('.diff-line-del', { hasText: '-two' })).toBeVisible()

  // Back to the list, then the clean repo shows the empty state.
  await page.getByTestId('repo-diff-back').click()
  await expect(page.getByTestId('repo-list-view')).toBeVisible()
  await page.locator('.list-view-name', { hasText: 'cli-repo' }).click()
  await expect(page.getByTestId('repo-diff-clean')).toBeVisible()

  // Deep link: reloading `/folders/<id>/repos` lands back on the repo
  // list for that folder (repo selection is view state, not URL).
  await page.reload()
  await expect(page.getByTestId('repo-list-view')).toBeVisible()
})
