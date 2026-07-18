/**
 * Preset first prompts offered by the New Session modal. Picking one
 * creates the session and immediately sends `build(...)` as its first
 * message (same create-then-message pattern as `installSession.ts`), so
 * the agent starts working without the user typing anything.
 *
 * `needsTopic` presets render an extra required text field in the modal
 * and receive its value in `build`. `requiresPlaywrightVideo` presets are
 * only offered while the `playwright-video` WASM plugin is installed and
 * approved — their prompt leans on the browser-run recording that plugin
 * replays.
 */
export interface PresetPrompt {
  id: string
  /** Option label in the modal's preset dropdown; also the auto session name. */
  label: string
  /** Extra required input rendered under the dropdown (only research today). */
  needsTopic?: boolean
  topicLabel?: string
  topicPlaceholder?: string
  /** Hide unless the playwright-video plugin is installed + approved. */
  requiresPlaywrightVideo?: boolean
  /** The first message. `topic` is '' for presets without `needsTopic`. */
  build: (topic: string) => string
}

export const PRESET_PROMPTS: PresetPrompt[] = [
  {
    id: 'ux-review',
    label: 'Recommend UX changes',
    build: () =>
      'Review this project from a UX perspective. Explore the user-facing surfaces — pages, flows, components, copy — and identify friction: confusing flows, missing affordances, inconsistent patterns, poor empty/error/loading states, and accessibility gaps. Recommend concrete UX changes ranked by impact, each pointing at the affected component or file. Do not change any code yet — deliver the recommendations first.',
  },
  {
    id: 'backend-improvements',
    label: 'Recommend backend improvements',
    build: () =>
      "Review this project's backend and recommend improvements. Look for performance problems (N+1 queries, missing indexes, blocking calls on hot paths), weak error handling, security gaps, brittle abstractions, and dead or duplicated code. Rank the recommendations by impact and include file references for each. Do not change any code yet — deliver the recommendations first.",
  },
  {
    id: 'bug-hunt',
    label: 'Hunt for bugs (browser)',
    requiresPlaywrightVideo: true,
    build: () =>
      "Launch this project's app and hunt for bugs by driving it in a real browser with the Playwright MCP tools. Traverse every reachable page and exercise the main flows — navigation, forms, dialogs, and edge-case inputs. Watch for console errors, failed network requests, broken navigation, layout glitches, and dead controls. For each bug found: reproduce it, then record the exact steps, its severity, and where in the code it likely lives. The browser runs are recorded for replay, so browse thoroughly rather than sampling. Report the full list when done.",
  },
  {
    id: 'feature-ideas',
    label: 'Recommend new features',
    build: () =>
      'Analyze this application: what it does, who uses it, and what its current features cover. Then recommend new features that would add the most value, ranked. For each one explain what it is, the user problem it solves, and a rough implementation sketch grounded in the existing architecture. Do not implement anything yet.',
  },
  {
    id: 'research',
    label: 'Research a topic',
    needsTopic: true,
    topicLabel: 'Topic',
    topicPlaceholder: 'What should I research?',
    build: (topic) =>
      `Research the following topic in depth: ${topic}\n\nUse web search where helpful, cross-check important claims across multiple sources, and produce a well-organized report with key findings, trade-offs, and cited sources.`,
  },
]

/** Auto session name when the Name field is hidden (temp) or left empty. */
export function presetSessionName(preset: PresetPrompt | undefined, topic: string): string {
  if (!preset) return 'Temp session'
  if (preset.needsTopic && topic.trim()) return `${preset.label}: ${topic.trim()}`
  return preset.label
}
