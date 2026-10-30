import { expect, type Locator, type Page } from '../harness'

/**
 * Open the voice conversation the way a user does: the rail's Listen button
 * (`voice-fab`) starts it and opens the full-page Assistant (`/assistant`),
 * whose transcript drawer hosts the voice panel. The drawer starts closed
 * (and inert), so open it, then hand back the visible panel.
 */
export async function openVoicePanel(page: Page): Promise<Locator> {
  const fab = page.getByTestId('voice-fab')
  await expect(fab).toBeVisible({ timeout: 10_000 })
  await fab.click()
  await expect(page).toHaveURL(/\/assistant$/)
  await expect(page.getByTestId('assistant-page')).toBeVisible()
  const toggle = page.getByTestId('assistant-transcript-toggle')
  if ((await toggle.getAttribute('aria-expanded')) !== 'true') await toggle.click()
  await expect(toggle).toHaveAttribute('aria-expanded', 'true')
  await expect(page.getByTestId('assistant-transcript')).toHaveAttribute('data-open', 'true')
  const panel = page.getByTestId('voice-panel')
  await expect(panel).toBeVisible()
  return panel
}

/** The panel's typed-fallback input. Its test id differs by host: the
 *  Assistant page's drawer passes `assistant-text-input`, the floating
 *  panel keeps `voice-type-input`. */
export function voiceTypeInput(page: Page): Locator {
  return page.getByTestId('voice-panel').locator('.voice-type-input')
}
