import { test, expect, type Page, type Route } from '@playwright/test'

/**
 * The Skills docs page (docs/skills.md) and its client-side catalog search
 * (docs/assets/js/skills-search.js). The public index and SKILL.md files
 * are served from mocks via route interception, so the spec never depends
 * on raw.githubusercontent.com.
 */

const RAW = 'https://raw.githubusercontent.com/PeckBoard/skills/main/'

const INDEX = {
  schema: 1,
  generated_at: '2026-01-01T00:00:00.000Z',
  skills: [
    {
      name: 'pr-review-checklist',
      description: 'Review a pull request and report findings by severity.',
      tags: ['review', 'git'],
      groups: ['pr-review'],
      versions: ['1.1.0', '1.0.0'],
      latest: '1.1.0',
    },
    {
      name: 'bug-repro',
      description: 'Turn a bug report into a failing test.',
      tags: ['debugging'],
      groups: ['testing'],
      versions: ['1.0.0'],
      latest: '1.0.0',
    },
    {
      name: 'changelog-writer',
      description: 'Write release notes from commits.',
      tags: ['release'],
      groups: [],
      versions: ['1.0.0'],
      latest: '1.0.0',
    },
  ],
  groups: [
    {
      slug: 'pr-review',
      name: 'PR review skills',
      description: '',
      skills: ['pr-review-checklist'],
    },
    { slug: 'testing', name: 'Testing skills', description: '', skills: ['bug-repro'] },
  ],
}

const skillFile = (name: string, version: string, body: string) =>
  `---\nname: ${name}\ndescription: x\nversion: ${version}\n---\n\n${body}\n`

async function mockCatalog(page: Page, opts: { failIndex?: boolean } = {}): Promise<string[]> {
  const fetched: string[] = []
  await page.route(`${RAW}**`, async (route: Route) => {
    const url = route.request().url()
    fetched.push(url)
    const rel = url.slice(RAW.length)
    if (rel === 'index.json') {
      if (opts.failIndex) return route.fulfill({ status: 503, body: 'down' })
      return route.fulfill({
        contentType: 'application/json',
        headers: { 'access-control-allow-origin': '*' },
        body: JSON.stringify(INDEX),
      })
    }
    const m = /^skills\/([a-z0-9-]+)\/([0-9.]+)\/SKILL\.md$/.exec(rel)
    if (m) {
      return route.fulfill({
        contentType: 'text/plain',
        headers: { 'access-control-allow-origin': '*' },
        body: skillFile(m[1], m[2], `# ${m[1]} body for ${m[2]}`),
      })
    }
    return route.fulfill({ status: 404, body: 'not found' })
  })
  return fetched
}

test.describe('Skills page', () => {
  test('renders with sidebar entry, links resolve, no Liquid leaks', async ({
    page,
    request,
    baseURL,
  }) => {
    await mockCatalog(page)
    await page.goto('skills.html')
    await expect(page).toHaveTitle(/PeckBoard/)
    await expect(page.locator('h1').first()).toContainText('Skills')
    for (const h of [
      'Local, Team, and Public Skills',
      'Groups',
      'Versions',
      'Team Repositories',
      'No Proprietary Content',
    ]) {
      await expect(page.locator('h2', { hasText: h }).first()).toBeVisible()
    }
    await expect(page.locator('code.language-mermaid svg').first()).toBeVisible({ timeout: 15_000 })
    // Listed in the sidebar of other doc pages.
    await page.goto('plugins.html')
    await expect(
      page.locator('.site-nav').getByRole('link', { name: 'Skills', exact: true }),
    ).toBeVisible()

    await page.goto('skills.html')
    const origin = new URL(baseURL!).origin
    const hrefs = await page.$$eval('a[href]', (as) => as.map((a) => (a as HTMLAnchorElement).href))
    for (const url of [...new Set(hrefs)]
      .filter((h) => h.startsWith(origin))
      .map((h) => h.split('#')[0])) {
      const res = await request.get(url)
      expect(res.status(), `broken link on skills: ${url}`).toBeLessThan(400)
    }
    const leak = await page.evaluate(() => {
      const clone = document.body.cloneNode(true) as HTMLElement
      clone.querySelectorAll('pre, code, script, style').forEach((el) => el.remove())
      const text = clone.textContent ?? ''
      const i = Math.max(text.indexOf('{{'), text.indexOf('{%'))
      return i >= 0 ? text.slice(Math.max(0, i - 40), i + 40) : null
    })
    expect(leak, `raw Liquid leaked: ${leak}`).toBeNull()
  })

  test('search filters by name, description, tag, and group', async ({ page }) => {
    await mockCatalog(page)
    await page.goto('skills.html')
    const cards = page.locator('[data-testid^="skill-card-"]')
    await expect(cards).toHaveCount(3)
    await expect(page.getByTestId('skills-status')).toHaveText('3 skills in the public catalog')

    const q = page.getByTestId('skills-q')
    await q.fill('failing test') // description words
    await expect(cards).toHaveCount(1)
    await expect(page.getByTestId('skill-card-bug-repro')).toBeVisible()
    await q.fill('release') // tag
    await expect(cards).toHaveCount(1)
    await expect(page.getByTestId('skill-card-changelog-writer')).toBeVisible()
    await q.fill('zzz-nothing')
    await expect(cards).toHaveCount(0)
    await expect(page.getByTestId('skills-status')).toHaveText('No skills match.')

    await q.fill('')
    const group = page.getByTestId('skills-group')
    await expect(group.locator('option')).toHaveText([
      'All groups',
      'PR review skills',
      'Testing skills',
    ])
    await group.selectOption('pr-review')
    await expect(cards).toHaveCount(1)
    await expect(page.getByTestId('skill-card-pr-review-checklist')).toContainText(
      'PR review skills',
    )
    await expect(page.getByTestId('skills-status')).toHaveText('1 of 3 skills')
  })

  test('expanding a skill fetches its SKILL.md and switches versions', async ({ page }) => {
    const fetched = await mockCatalog(page)
    await page.goto('skills.html')
    const toggle = page.getByTestId('skill-toggle-pr-review-checklist')
    await expect(toggle).toHaveAttribute('aria-expanded', 'false')
    await toggle.click()
    await expect(toggle).toHaveAttribute('aria-expanded', 'true')
    const body = page.getByTestId('skill-body-pr-review-checklist')
    await expect(body).toHaveText('# pr-review-checklist body for 1.1.0')
    // Frontmatter is stripped from the shown body.
    await expect(body).not.toContainText('version:')
    expect(fetched).toContain(`${RAW}skills/pr-review-checklist/1.1.0/SKILL.md`)

    const versions = page.getByTestId('skill-versions-pr-review-checklist')
    await expect(versions.locator('option')).toHaveText(['1.1.0 (latest)', '1.0.0'])
    await versions.selectOption('1.0.0')
    await expect(body).toHaveText('# pr-review-checklist body for 1.0.0')

    await toggle.click()
    await expect(body).toBeHidden()
  })

  test('subscribe link uses the saved instance address, persisted across reloads', async ({
    page,
  }) => {
    await mockCatalog(page)
    await page.goto('skills.html')
    await page.getByTestId('skill-toggle-bug-repro').click()
    const sub = page.getByTestId('skill-subscribe-bug-repro')
    await expect(sub).toHaveAttribute('data-needs-instance', 'true')
    // Without an address, clicking asks for one instead of navigating.
    await sub.click()
    await expect(page.getByTestId('skills-instance-url')).toBeFocused()
    await expect(page.getByTestId('skills-instance-status')).toContainText(
      'Enter your PeckBoard address',
    )

    // Invalid scheme is rejected.
    await page.getByTestId('skills-instance-url').fill('ftp://box.example.com')
    await page.getByTestId('skills-instance-save').click()
    await expect(page.getByTestId('skills-instance-status')).toHaveText(
      'That is not an http(s) address.',
    )

    // Bare host gets https:// and loses its trailing slash.
    await page.getByTestId('skills-instance-url').fill('peckboard.example.com:3345/')
    await page.getByTestId('skills-instance-save').click()
    await expect(sub).toHaveAttribute(
      'href',
      'https://peckboard.example.com:3345/settings?tab=skills&subscribe=bug-repro',
    )
    await expect(sub).not.toHaveAttribute('data-needs-instance', 'true')

    await page.reload()
    await expect(page.getByTestId('skills-instance-url')).toHaveValue(
      'https://peckboard.example.com:3345',
    )
    await expect(page.getByTestId('skill-subscribe-pr-review-checklist')).toHaveAttribute(
      'href',
      'https://peckboard.example.com:3345/settings?tab=skills&subscribe=pr-review-checklist',
    )
  })

  test('a failed index load shows an error with a GitHub fallback', async ({ page }) => {
    await mockCatalog(page, { failIndex: true })
    await page.goto('skills.html')
    const status = page.getByTestId('skills-status')
    await expect(status).toContainText('Could not load the public catalog (HTTP 503)')
    await expect(status.getByRole('link', { name: 'Browse it on GitHub' })).toHaveAttribute(
      'href',
      'https://github.com/PeckBoard/skills',
    )
    await expect(page.locator('[data-testid^="skill-card-"]')).toHaveCount(0)
  })
})
