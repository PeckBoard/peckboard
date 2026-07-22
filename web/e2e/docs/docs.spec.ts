import { test, expect } from '@playwright/test'

/**
 * Rendering checks for the public docs site, served from docs/_site under
 * the production /peckboard baseurl (see serve.mjs). Every page must
 * render with its title and sidebar, internal links and images must
 * resolve, Mermaid diagrams must render to SVG, and collapsible blocks
 * must toggle.
 */

// path (relative to baseURL) → sidebar/page title
const PAGES: Array<{ path: string; title: string }> = [
  { path: '', title: 'Home' },
  { path: 'getting-started.html', title: 'Getting Started' },
  { path: 'core-concepts.html', title: 'Core Concepts' },
  { path: 'experts.html', title: 'Experts' },
  { path: 'plugins.html', title: 'Plugins' },
  { path: 'architecture.html', title: 'Architecture' },
  { path: 'configuration.html', title: 'Configuration' },
  { path: 'style-guide.html', title: 'Style Guide' },
]

// Pages whose source contains ```mermaid blocks.
const MERMAID_PAGES = [
  'core-concepts.html',
  'plugins.html',
  'session-hooks.html',
  'architecture.html',
  'style-guide.html',
]

for (const { path: pagePath, title } of PAGES) {
  test.describe(title, () => {
    test.beforeEach(async ({ page }) => {
      await page.goto(pagePath)
    })

    test('renders heading, title, and sidebar nav', async ({ page }) => {
      await expect(page).toHaveTitle(new RegExp('PeckBoard'))
      const h1 = page.locator('h1').first()
      await expect(h1).toBeVisible()
      if (pagePath !== '') await expect(h1).toContainText(title)
      // The sidebar lists every public page.
      const nav = page.locator('.site-nav, nav[aria-label="Main"]').first()
      await expect(nav).toBeVisible()
      for (const other of PAGES) {
        if (other.title === title) {
          // The current page's nav item is not exposed as a link.
          await expect(nav.getByText(other.title, { exact: true }).first()).toBeVisible()
        } else {
          await expect(nav.getByRole('link', { name: other.title, exact: true })).toBeVisible()
        }
      }
    })
    test('no unrendered Liquid outside code blocks', async ({ page }) => {
      const leak = await page.evaluate(() => {
        const clone = document.body.cloneNode(true) as HTMLElement
        clone.querySelectorAll('pre, code, script, style').forEach((el) => el.remove())
        const text = clone.textContent ?? ''
        for (const marker of ['{{', '{%']) {
          const i = text.indexOf(marker)
          if (i >= 0) return text.slice(Math.max(0, i - 40), i + 40)
        }
        return null
      })
      expect(leak, `raw Liquid leaked into rendered text: ${leak}`).toBeNull()
    })

    test('internal links resolve', async ({ page, request, baseURL }) => {
      const hrefs = await page.$$eval('a[href]', (as) =>
        as.map((a) => (a as HTMLAnchorElement).href),
      )
      const origin = new URL(baseURL!).origin
      const internal = [...new Set(hrefs)]
        .filter((h) => h.startsWith(origin))
        .map((h) => h.split('#')[0])
        .filter((h) => h.length > 0)
      expect(internal.length).toBeGreaterThan(0)
      for (const url of internal) {
        const res = await request.get(url)
        expect(res.status(), `broken link on ${pagePath || 'index'}: ${url}`).toBeLessThan(400)
      }
    })

    test('images load', async ({ page }) => {
      const bad = await page.evaluate(async () => {
        const imgs = Array.from(document.querySelectorAll('img'))
        await Promise.all(
          imgs.map((img) =>
            img.complete
              ? null
              : new Promise((r) => img.addEventListener('load', r, { once: true })),
          ),
        )
        return imgs.filter((i) => i.naturalWidth === 0).map((i) => i.src)
      })
      expect(bad, `images failed to load: ${bad.join(', ')}`).toEqual([])
    })
  })
}

for (const pagePath of MERMAID_PAGES) {
  test(`mermaid diagrams render as SVG on ${pagePath}`, async ({ page }) => {
    await page.goto(pagePath)
    // just-the-docs renders the SVG in place, inside code.language-mermaid.
    const diagrams = page.locator('code.language-mermaid')
    const count = await diagrams.count()
    expect(count, 'expected at least one mermaid block').toBeGreaterThan(0)
    for (let i = 0; i < count; i++) {
      await expect(diagrams.nth(i).locator('svg')).toBeVisible({ timeout: 15_000 })
    }
    if (pagePath === 'session-hooks.html') {
      // Liquid must not eat the {{hook.name}} node labels ({% raw %}-wrapped).
      await expect(diagrams.first().locator('svg')).toContainText('session.message.before')
    }
  })
}

test('collapsible details blocks toggle open', async ({ page }) => {
  await page.goto('getting-started.html')
  const details = page.locator('details').first()
  await expect(details).not.toHaveAttribute('open', '')
  await details.locator('summary').click()
  await expect(details).toHaveAttribute('open', '')
})

test('search finds core pages', async ({ page }) => {
  await page.goto('')
  const input = page.locator('#search-input, .search-input').first()
  await expect(input).toBeVisible()
  // The theme's search only reacts to real keystrokes (fill()'s synthetic
  // input event is ignored), and activates once search-data.json loads.
  await expect(async () => {
    await input.fill('')
    await input.pressSequentially('worker', { delay: 25 })
    await expect(page.locator('.search-result').first()).toBeVisible({ timeout: 2_000 })
  }).toPass({ timeout: 20_000 })
})

// session-hooks.html is a child page of Plugins: its sidebar link is only
// expanded while the Plugins section is active, so it stays out of the PAGES
// loop (whose nav assertion runs on every page) and is checked here instead.
test.describe('Session Hooks (child page)', () => {
  test('renders with nav link visible under its parent', async ({ page }) => {
    await page.goto('session-hooks.html')
    await expect(page.locator('h1').first()).toContainText('Session Hooks')
    const nav = page.locator('.site-nav').first()
    await expect(nav.getByText('Session Hooks', { exact: true }).first()).toBeVisible()
    // Parent page shows the child link too.
    await page.goto('plugins.html')
    await expect(
      page.locator('.site-nav').getByRole('link', { name: 'Session Hooks' }),
    ).toBeVisible()
  })

  test('internal links resolve and no Liquid leaks', async ({ page, request, baseURL }) => {
    await page.goto('session-hooks.html')
    const hrefs = await page.$$eval('a[href]', (as) => as.map((a) => (a as HTMLAnchorElement).href))
    const origin = new URL(baseURL!).origin
    const internal = [...new Set(hrefs)]
      .filter((h) => h.startsWith(origin))
      .map((h) => h.split('#')[0])
    for (const url of internal) {
      const res = await request.get(url)
      expect(res.status(), `broken link on session-hooks: ${url}`).toBeLessThan(400)
    }
    const leak = await page.evaluate(() => {
      const clone = document.body.cloneNode(true) as HTMLElement
      clone.querySelectorAll('pre, code, script, style').forEach((el) => el.remove())
      const text = clone.textContent ?? ''
      const i = text.indexOf('{{')
      return i >= 0 ? text.slice(Math.max(0, i - 40), i + 40) : null
    })
    expect(leak, `raw Liquid leaked: ${leak}`).toBeNull()
  })
})
