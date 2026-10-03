import { test, expect, type APIRequestContext, type Page } from '../harness'

/**
 * Inside the PeckBoard mobile app (UA suffix `PeckBoardApp/<v>`), the user
 * menu offers "Switch box", which sends the WebView back to the app shell
 * whose origin the app's loopback gate stored in the `__pbm_shell` cookie.
 * The shell (`http://tauri.localhost`) is mocked with a routed page.
 */

const E2E_USER = process.env.PECKBOARD_E2E_USER ?? 'e2e-user'
const E2E_PASS = process.env.PECKBOARD_E2E_PASS ?? 'e2e-password-1234'
const IOS_UA =
  'Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148'
const SHELL = 'http://tauri.localhost'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok()).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

async function openUserMenu(page: Page, token: string, baseURL: string, shell: string) {
  await page.context().addCookies([{ name: '__pbm_shell', value: shell, url: baseURL }])
  await page.addInitScript((t) => {
    localStorage.setItem('peckboard_token', t)
  }, token)
  await page.goto('/')
  await expect(page.locator('.tabbar')).toBeVisible({ timeout: 10_000 })
  await page.getByRole('button', { name: 'User menu' }).click()
  const menu = page.locator('.user-menu-dropdown')
  await expect(menu.getByRole('menuitem', { name: 'Sign out' })).toBeVisible()
  return menu
}

test.describe('mobile app: Switch box', () => {
  test.describe('inside the app', () => {
    test.use({ viewport: { width: 390, height: 844 }, userAgent: `${IOS_UA} PeckBoardApp/0.1.0` })

    test('returns to the app shell', async ({ request, page, baseURL }) => {
      await page.route(`${SHELL}/**`, (route) =>
        route.fulfill({ contentType: 'text/html', body: '<h1>Your boxes</h1>' }),
      )
      const menu = await openUserMenu(page, await authenticate(request), baseURL!, SHELL)
      await menu.getByRole('menuitem', { name: 'Switch box' }).click()
      await expect(page.getByRole('heading', { name: 'Your boxes' })).toBeVisible()
      expect(page.url()).toBe(`${SHELL}/`)
    })

    test('ignores a shell cookie that is not a Tauri origin', async ({
      request,
      page,
      baseURL,
    }) => {
      const menu = await openUserMenu(
        page,
        await authenticate(request),
        baseURL!,
        'https://evil.example',
      )
      await expect(menu.getByRole('menuitem', { name: 'Switch box' })).toHaveCount(0)
    })
  })

  test('absent in a plain browser', async ({ request, page, baseURL }) => {
    const menu = await openUserMenu(page, await authenticate(request), baseURL!, SHELL)
    await expect(menu.getByRole('menuitem', { name: 'Switch box' })).toHaveCount(0)
  })
})
