/**
 * Contract between the full-page Assistant view (graphics:
 * `web/src/components/assistant/**`) and its data/wiring layer
 * (`web/src/store/assistantTopics.ts`, `web/src/voice/levels.ts`).
 *
 * The view only reads these shapes and calls these functions; the wiring
 * layer implements them. Keep both sides to this file.
 */

import type { ReactElement } from 'react'

/** What a topic is about. The assistant model curates the WHOLE ring
 *  through its `assistant_topics` tool — nothing is derived automatically:
 *  - `conversation`: a thread of the conversation itself;
 *  - `session` / `project`: a piece of live work the assistant chose to put
 *    on the ring; it links to that session/project, and the server fills
 *    in its live `status`.
 *  All are stored per voice session. */
export type TopicKind = 'conversation' | 'session' | 'project'

export interface AssistantTopic {
  /** Stable id (the stored row id). */
  id: string
  title: string
  /** One line for the tooltip / expanded view. */
  summary: string | null
  /** For `session` / `project`: the id it points at (to link to it). */
  refId: string | null
  /** For `session` / `project`: a short live status, e.g. "running",
   *  "waiting on you", "paused". */
  status: string | null
  /** ISO time of the last change — newest first when the ring is full. */
  updatedAt: string
}

export interface AssistantTopicsState {
  /** Every topic on the ring (excluding none — the current one is in here
   *  too, flagged by `currentId`). */
  topics: AssistantTopic[]
  /** The topic the conversation is about right now, or null. When it
   *  changes, the view plays the "absorb" animation for that topic. */
  currentId: string | null
  loading: boolean
  error: string | null
  /** Load (and keep live over WS) the topics of the voice session. */
  load(): Promise<void>
  /** The user picked a topic (click). Makes it current on the server and
   *  tells the assistant ("Let's talk about <title>"). Resolves once the
   *  server accepted it; `currentId` updates through the same path as a
   *  voice-driven switch. */
  focus(id: string): Promise<void>
}

/** What the orb shows. Mirrors the voice store's state machine. */
export type OrbState = 'offline' | 'idle' | 'listening' | 'thinking' | 'speaking'

/** Live audio levels for the orb, each 0..1, smoothed. Read every
 *  animation frame — must be cheap and never throw. `input` is the user's
 *  mic (0 when not listening or unavailable), `output` the assistant's
 *  voice (0 when silent). */
export interface AudioLevels {
  input(): number
  output(): number
}

/** The full-page Assistant lives at this route; the voice button opens it. */
export const ASSISTANT_ROUTE = '/assistant'

/** Default export of `web/src/components/assistant/AssistantPage.tsx`,
 *  rendered by the router at `ASSISTANT_ROUTE`. Takes no props. */
export type AssistantPageComponent = () => ReactElement

/**
 * `data-testid`s the e2e specs rely on (both sides must keep them):
 * - `assistant-page`            the page root
 * - `assistant-status-dot`      top-left status dot; `data-state` = OrbState
 * - `assistant-orb`             the orb; `data-state` = OrbState
 * - `assistant-current-topic`   the current topic's label inside the orb
 *                               (absent when `currentId` is null)
 * - `assistant-topic`           each ring topic (a button); `data-topic-id`,
 *                               `data-kind`; the current one is NOT on the ring
 * - `assistant-absorbing`       present only while the absorb animation runs
 * - `assistant-listen`          start/stop listening (mic) button
 * - `assistant-transcript-toggle` / `assistant-transcript`  the drawer
 * - `assistant-text-input`      typed fallback input in the drawer
 */
export const ASSISTANT_TEST_IDS = [
  'assistant-page',
  'assistant-status-dot',
  'assistant-orb',
  'assistant-current-topic',
  'assistant-topic',
  'assistant-absorbing',
  'assistant-listen',
  'assistant-transcript-toggle',
  'assistant-transcript',
  'assistant-text-input',
] as const
