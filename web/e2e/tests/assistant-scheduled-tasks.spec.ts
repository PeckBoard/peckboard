import { test, expect, type APIRequestContext } from '../harness'
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * Repeating tasks that run in the Assistant.
 *
 * A task created in the New Repeating Task modal with "Runs in: Assistant"
 * shows an Assistant badge in the list; "Run now" queues it into the voice
 * relay gate instead of spawning a session, and the Assistant (here
 * `mock:echo`, which echoes its input) answers the `[relay] scheduled task`
 * turn — visible in the Assistant page's transcript.
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  const authHeader = { Authorization: `Bearer ${token}` }
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-assistant-sched-'))
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: `assistant-sched-${path.basename(folderPath)}`, path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  return { token, authHeader, folderId: folder.id }
}

test('an Assistant-target task runs in the Assistant and its reply shows on the Assistant page', async ({
  request,
  page,
}) => {
  const { token, authHeader, folderId } = await authenticate(request)
  const voiceRes = await request.post('/api/voice/session', {
    headers: authHeader,
    data: { model: 'mock:echo' },
  })
  expect(voiceRes.ok(), await voiceRes.text()).toBeTruthy()
  const voice = (await voiceRes.json()) as { session_id: string }
  const taskName = `pangolin check ${Date.now()}`
  let taskId: string | undefined

  try {
    await page.addInitScript((t) => {
      localStorage.setItem('peckboard_token', t)
      // A silent recognizer and no real speech output in the test browser.
      class FakeRecognition {
        lang = ''
        continuous = false
        interimResults = false
        maxAlternatives = 1
        onresult: unknown = null
        onerror: unknown = null
        onend: (() => void) | null = null
        start() {}
        stop() {
          this.onend?.()
        }
        abort() {
          this.onend?.()
        }
      }
      const w = window as unknown as Record<string, unknown>
      w.SpeechRecognition = FakeRecognition
      w.webkitSpeechRecognition = FakeRecognition
      Object.defineProperty(window, 'speechSynthesis', {
        value: {
          speak: (u: { onend?: () => void }) => setTimeout(() => u.onend?.(), 5),
          cancel: () => {},
          pause: () => {},
          resume: () => {},
          getVoices: () => [],
          addEventListener: () => {},
          removeEventListener: () => {},
          speaking: false,
          pending: false,
          paused: false,
        },
        configurable: true,
      })
    }, token)
    await page.goto('/repeating-tasks')
    await expect(page.getByRole('heading', { name: 'Repeating Tasks' })).toBeVisible()

    // Create it in the modal, running in the Assistant.
    await page.getByRole('button', { name: /new task/i }).click()
    await expect(page.getByRole('heading', { name: /new repeating task/i })).toBeVisible()
    await page.getByPlaceholder(/daily project sweep/i).fill(taskName)
    await page.locator('#repeating-task-folder').selectOption({ value: folderId })
    await page.getByTestId('repeating-task-target').selectOption('assistant')
    // Assistant runs don't take a model: the session-only fields go away.
    await expect(page.getByTestId('repeating-task-model')).toHaveCount(0)
    await page.locator('#repeating-task-prompt').fill('Say PANGOLIN out loud.')
    await page.getByRole('button', { name: /create task/i }).click()
    await expect(page.getByRole('heading', { name: /new repeating task/i })).toHaveCount(0)

    // Saving opens the task: it runs in the Assistant…
    await expect(page.getByRole('heading', { name: taskName })).toBeVisible()
    await expect(page.locator('.repeating-task-meta dt:text-is("Runs in") + dd')).toHaveText(
      'Assistant',
    )
    // …and its list row carries the Assistant badge.
    await page.getByRole('button', { name: '← Back' }).click()
    const row = page.locator('.list-view-row', { hasText: taskName })
    await expect(row.getByTestId('repeating-task-assistant-badge')).toHaveText('Assistant')
    const tasks = (await (
      await request.get('/api/repeating-tasks', { headers: authHeader })
    ).json()) as { id: string; name: string; target?: string | null }[]
    const task = tasks.find((t) => t.name === taskName)!
    taskId = task.id
    expect(task.target).toBe('assistant')

    // Run now: queued for the Assistant, no session spawned.
    await row.getByText(taskName).click()
    await page.getByRole('button', { name: 'Run now' }).click()
    await expect(page.locator('.repeating-task-run-status')).toContainText(
      'Queued for the Assistant',
    )
    const sessions = (await (
      await request.get(`/api/repeating-tasks/${taskId}/sessions`, { headers: authHeader })
    ).json()) as unknown[]
    expect(sessions).toHaveLength(0)

    // The Assistant performs it; its reply is on the Assistant page.
    await page.goto('/assistant')
    await expect(page.getByTestId('assistant-page')).toBeVisible()
    await page.getByTestId('assistant-transcript-toggle').click()
    const transcript = page.getByTestId('assistant-transcript')
    await expect(transcript.getByTestId('voice-line-assistant').last()).toContainText('PANGOLIN', {
      timeout: 20_000,
    })
  } finally {
    if (taskId) {
      await request.delete(`/api/repeating-tasks/${taskId}`, { headers: authHeader })
    }
    await request.post(`/api/sessions/${voice.session_id}/cancel`, { headers: authHeader })
    // The voice session is shared with the other voice specs: leave its
    // transcript empty so their "first user line" checks hold.
    await request.post(`/api/sessions/${voice.session_id}/clear`, { headers: authHeader })
  }
})
