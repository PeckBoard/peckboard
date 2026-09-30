import { test, expect, type APIRequestContext, type Page, type Request } from '../harness'

/**
 * Settings → Voice → Pronunciations: the server-side Kokoro lexicon.
 *
 * Drives the real lexicon routes (list, preview, PUT, DELETE) end to end.
 * `POST /api/voice/tts` is stubbed with a tiny silent WAV so no Kokoro
 * model is downloaded — the tests assert what the page *asked* to speak
 * (the word plus the previewed phonemes). The unknown-words list is only
 * filled by real synthesis, so that test stubs its two routes too.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext): Promise<string> {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return token
}

/** 8 kHz mono 16-bit PCM, 80 samples of silence. */
function tinyWav(): Buffer {
  const samples = 80
  const buf = Buffer.alloc(44 + samples * 2)
  buf.write('RIFF', 0)
  buf.writeUInt32LE(36 + samples * 2, 4)
  buf.write('WAVE', 8)
  buf.write('fmt ', 12)
  buf.writeUInt32LE(16, 16)
  buf.writeUInt16LE(1, 20)
  buf.writeUInt16LE(1, 22)
  buf.writeUInt32LE(8000, 24)
  buf.writeUInt32LE(16000, 28)
  buf.writeUInt16LE(2, 32)
  buf.writeUInt16LE(16, 34)
  buf.write('data', 36)
  buf.writeUInt32LE(samples * 2, 40)
  return buf
}

/** Stub TTS and collect every request body the page sends to it. */
async function stubTts(page: Page): Promise<Record<string, unknown>[]> {
  const bodies: Record<string, unknown>[] = []
  await page.route('**/api/voice/tts', async (route) => {
    bodies.push(route.request().postDataJSON() as Record<string, unknown>)
    await route.fulfill({ status: 200, contentType: 'audio/wav', body: tinyWav() })
  })
  return bodies
}

async function openPronunciations(page: Page, token: string) {
  await page.addInitScript((t) => localStorage.setItem('peckboard_token', t), token)
  await page.goto('/settings/voice')
  const section = page.getByTestId('voice-pronunciations-section')
  await expect(section).toBeVisible({ timeout: 10_000 })
  return section
}

/** A letters-only word no other run has used (server state is shared). */
function uniqueWord(): string {
  const tail = Array.from({ length: 5 }, () =>
    String.fromCharCode(97 + Math.floor(Math.random() * 26)),
  ).join('')
  return `Zorb${tail}`
}

test('add, preview, play, edit and delete a pronunciation', async ({ request, page }) => {
  const token = await authenticate(request)
  const ttsBodies = await stubTts(page)
  const section = await openPronunciations(page, token)
  const rows = section.locator('.list-view-row')

  // ── Seeded defaults are listed and tagged as such ─────────────────
  const peck = rows.filter({ hasText: 'Peckboard' }).first()
  await expect(peck).toBeVisible({ timeout: 10_000 })
  await expect(peck).toContainText('default')
  await expect(rows.filter({ hasText: 'Kokoro' }).first()).toContainText('default')

  // ── Add: a respelling previews to phonemes, and Play speaks them ──
  const word = uniqueWord()
  await section.getByTestId('voice-lexicon-add').click()
  const modal = page.getByTestId('voice-lexicon-modal')
  await expect(modal).toBeVisible()
  await expect(modal.getByTestId('voice-lexicon-save')).toBeDisabled()
  await expect(modal.getByTestId('voice-lexicon-disabled-reason')).toHaveText('Enter the word.')

  await modal.getByTestId('voice-lexicon-word').fill(word)
  await modal.getByTestId('voice-lexicon-text').fill('ZORB-blax')
  const preview = modal.getByTestId('voice-lexicon-preview')
  await expect(preview).not.toHaveText('…', { timeout: 10_000 })
  const phonemes = (await preview.textContent())!.trim()
  expect(phonemes.length).toBeGreaterThan(0)
  await expect(modal.getByTestId('voice-lexicon-save')).toBeEnabled()

  await modal.getByTestId('voice-lexicon-draft-play').click()
  await expect.poll(() => ttsBodies.length).toBe(1)
  expect(ttsBodies[0]).toMatchObject({ text: word, phonemes })
  expect(typeof ttsBodies[0].voice).toBe('string')

  await modal.getByTestId('voice-lexicon-save').click()
  await expect(modal).toBeHidden()
  const row = rows.filter({ hasText: word })
  await expect(row).toBeVisible()
  await expect(row).toContainText('custom')
  await expect(row).toContainText('ZORB-blax')

  // ── Play a saved entry: the word alone, the server applies the lexicon
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Play' }).click()
  await expect.poll(() => ttsBodies.length).toBe(2)
  expect(ttsBodies[1]).toMatchObject({ text: word })
  expect(ttsBodies[1].phonemes).toBeUndefined()

  // ── Edit: the dialog opens prefilled, the word is locked ──────────
  await row.locator('.list-view-item').click()
  await expect(modal).toBeVisible()
  await expect(modal.getByTestId('voice-lexicon-word')).toBeDisabled()
  await expect(modal.getByTestId('voice-lexicon-text')).toHaveValue('ZORB-blax')
  await modal.getByTestId('voice-lexicon-text').fill('zorb-BLAX')
  await expect(modal.getByTestId('voice-lexicon-save')).toBeEnabled({ timeout: 10_000 })
  await modal.getByTestId('voice-lexicon-save').click()
  await expect(modal).toBeHidden()
  await expect(row).toContainText('zorb-BLAX')

  // ── Delete, through the shared confirm ────────────────────────────
  await row.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Delete' }).click()
  const confirm = page.getByTestId('voice-lexicon-delete-confirm')
  await expect(confirm).toBeVisible()
  await confirm.getByTestId('confirm-dialog-confirm').click()
  await expect(confirm).toBeHidden()
  await expect(rows.filter({ hasText: word })).toHaveCount(0)
})

test('an invalid word or phonemes show inline errors and block Save', async ({ request, page }) => {
  const token = await authenticate(request)
  await stubTts(page)
  const section = await openPronunciations(page, token)

  await section.getByTestId('voice-lexicon-add').click()
  const modal = page.getByTestId('voice-lexicon-modal')
  const wordInput = modal.getByTestId('voice-lexicon-word')

  // Entries are single words; a phrase is caught before any round trip.
  await wordInput.fill('two words')
  await expect(modal.getByTestId('voice-lexicon-word-error')).toHaveText(
    'One word, letters and digits only.',
  )
  await expect(modal.getByTestId('voice-lexicon-disabled-reason')).toHaveText('Fix the word first.')
  // Route names can't be entry keys.
  await wordInput.fill('Unknown')
  await expect(modal.getByTestId('voice-lexicon-word-error')).toContainText('reserved')

  await wordInput.fill(uniqueWord())
  await expect(modal.getByTestId('voice-lexicon-word-error')).toHaveCount(0)
  await modal.getByTestId('voice-lexicon-mode-phonemes').click()
  await modal.getByTestId('voice-lexicon-text').fill('p%&123')

  await expect(modal.getByTestId('voice-lexicon-text-error')).toBeVisible({ timeout: 10_000 })
  await expect(modal.getByTestId('voice-lexicon-save')).toBeDisabled()
  await expect(modal.getByTestId('voice-lexicon-disabled-reason')).toHaveText(
    'Fix the pronunciation first.',
  )
  await expect(modal.getByTestId('voice-lexicon-draft-play')).toHaveCount(0)
})

test('unknown words: add a pronunciation or dismiss', async ({ request, page }) => {
  const token = await authenticate(request)
  await stubTts(page)
  const now = new Date().toISOString()
  let unknown = [{ word: 'frobnitz', count: 3, first_seen: now, last_seen: now }]
  const dismissed: Request[] = []
  await page.route(/\/api\/voice\/lexicon\/unknown/, async (route) => {
    if (route.request().method() === 'DELETE') {
      dismissed.push(route.request())
      unknown = []
      await route.fulfill({ status: 204 })
    } else {
      await route.fulfill({ status: 200, json: unknown })
    }
  })
  const section = await openPronunciations(page, token)

  const row = section.getByTestId('voice-unknown-row-frobnitz')
  await expect(row).toBeVisible()
  const listRow = section
    .locator('.list-view-row')
    .filter({ has: page.getByTestId('voice-unknown-row-frobnitz') })
  await expect(listRow).toContainText('3× heard')

  // Add pronunciation opens the form with the word already filled in.
  await listRow.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Add pronunciation' }).click()
  const modal = page.getByTestId('voice-lexicon-modal')
  await expect(modal.getByTestId('voice-lexicon-word')).toHaveValue('frobnitz')
  await expect(modal.getByTestId('voice-lexicon-text')).toBeFocused()
  await modal.getByRole('button', { name: 'Cancel' }).click()
  await expect(modal).toBeHidden()

  // Dismiss drops it from the list and tells the server.
  await listRow.locator('.list-view-menu').click()
  await page.getByRole('menuitem', { name: 'Dismiss' }).click()
  await expect(row).toHaveCount(0)
  expect(dismissed).toHaveLength(1)
  expect(dismissed[0].url()).toMatch(/\/api\/voice\/lexicon\/unknown\/frobnitz$/)
  await expect(section.getByTestId('voice-unknown-empty')).toBeVisible()
})
