import { test, expect, type APIRequestContext, type Page } from '@playwright/test'

/**
 * Design-token contrast guard (WCAG 2.2 AA).
 *
 * `--ring` is the ONLY focus affordance in the app (`index.css`:
 * `:focus-visible { outline: 2px solid var(--ring) }`) and `--text3` is the
 * muted-text token used ~160 times. Both used to fail AA (light `--ring`
 * was 1.80:1, light `--text3` 2.54:1).
 *
 * `--ring` is derived from `--primary-hue`, which the user drives with a free
 * 0-360 slider, so the ratio is checked at every 30 degrees — a value that
 * passes at the default hue 220 can still be invisible at hue 60 (yellow).
 *
 * Ratios are computed in-page from the *resolved* colors: every token is read
 * back through a probe element's computed `color`, so `hsl(var(--…))` is
 * substituted by the browser rather than parsed here.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// WCAG 2.2: 3:1 for UI components / focus indicators, 4.5:1 for normal text.
const UI_MIN = 3.0
const TEXT_MIN = 4.5

const HUES = [0, 30, 60, 90, 120, 150, 180, 210, 220, 240, 270, 300, 330, 360]

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function loadAt(page: Page, token: string, route: string) {
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto(route)
}

type Probe = { theme: string; hue: number; ratios: Record<string, number> }

/** Resolve tokens and compute contrast ratios for one (theme, hue). */
async function measure(page: Page, theme: 'light' | 'dark', hue: number): Promise<Probe> {
  const ratios = await page.evaluate(
    ({ theme, hue }) => {
      const root = document.documentElement
      root.setAttribute('data-theme', theme)
      root.style.setProperty('--primary-hue', String(hue))

      const probe = document.createElement('span')
      probe.style.position = 'fixed'
      probe.style.opacity = '0'
      root.appendChild(probe)

      const resolve = (token: string): [number, number, number] => {
        probe.style.color = ''
        probe.style.color = `var(${token})`
        const m = getComputedStyle(probe).color.match(/[\d.]+/g)
        if (!m) throw new Error(`could not resolve ${token}`)
        return [Number(m[0]), Number(m[1]), Number(m[2])]
      }

      const luminance = ([r, g, b]: [number, number, number]) => {
        const lin = (c: number) => {
          const s = c / 255
          return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
      }

      const ratio = (a: string, b: string) => {
        const [hi, lo] = [luminance(resolve(a)), luminance(resolve(b))].sort((x, y) => y - x)
        return (hi + 0.05) / (lo + 0.05)
      }

      const out: Record<string, number> = {}
      for (const surface of ['--surface', '--surface2', '--surface3']) {
        out[`ring/${surface}`] = ratio('--ring', surface)
        out[`text3/${surface}`] = ratio('--text3', surface)
      }
      probe.remove()
      return out
    },
    { theme, hue },
  )
  return { theme, hue, ratios }
}

for (const theme of ['light', 'dark'] as const) {
  test(`${theme} theme: --ring and --text3 meet WCAG AA at every accent hue`, async ({
    request,
    page,
  }) => {
    const token = await authenticate(request)
    await loadAt(page, token, '/')
    await page.waitForSelector('.rail-btn')

    for (const hue of HUES) {
      const { ratios } = await measure(page, theme, hue)
      for (const surface of ['--surface', '--surface2', '--surface3']) {
        expect(
          ratios[`ring/${surface}`],
          `--ring on ${surface} at hue ${hue}`,
        ).toBeGreaterThanOrEqual(UI_MIN)
        expect(
          ratios[`text3/${surface}`],
          `--text3 on ${surface} at hue ${hue}`,
        ).toBeGreaterThanOrEqual(TEXT_MIN)
      }
    }
  })
}

test('the rail icons sit at --text2, not the muted --text3', async ({ request, page }) => {
  const token = await authenticate(request)
  await loadAt(page, token, '/')
  await page.waitForSelector('.rail-btn')

  const { rail, text2, text3 } = await page.evaluate(() => {
    const probe = document.createElement('span')
    document.documentElement.appendChild(probe)
    const read = (token: string) => {
      probe.style.color = `var(${token})`
      return getComputedStyle(probe).color
    }
    const text2 = read('--text2')
    const text3 = read('--text3')
    probe.remove()
    const btn = document.querySelector('.rail-btn:not(.active)')
    if (!btn) throw new Error('no inactive .rail-btn on the page')
    return { rail: getComputedStyle(btn).color, text2, text3 }
  })

  expect(rail).toBe(text2)
  expect(rail).not.toBe(text3)
})
