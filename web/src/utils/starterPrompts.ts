/** A use case a user can say they care about; orders the starter prompts. */
export type UseCase = 'coding' | 'writing' | 'ops' | 'printing'

/** One clickable suggestion shown in a chat that has no messages yet. */
export interface StarterPrompt {
  id: string
  title: string
  /** One line under the title: what the prompt shows off. */
  description: string
  /** Text placed in the composer. The user reviews it and presses Send. */
  prompt: string
  tags: UseCase[]
  /** Only offered while this plugin is installed, enabled and not
   *  experimental. Omitted ⇒ always offered. */
  plugin?: string
}

/** Most starter cards a chat shows at once. */
export const MAX_STARTER_PROMPTS = 6

/** Static catalog. General capabilities first, then plugin extras. Never add
 *  a HomeKit prompt: device control is not a first-run demo. */
export const STARTER_PROMPTS: StarterPrompt[] = [
  {
    id: 'explain-project',
    title: 'Explain this project',
    description: 'Get a plain tour of the files in this folder.',
    prompt:
      'Explain this project to me: what it does, how the code is laid out, and where I should start reading.',
    tags: ['coding', 'writing'],
  },
  {
    id: 'quick-mockup',
    title: 'Make a quick mockup',
    description: 'See an HTML mockup open right next to the chat.',
    prompt:
      'Make a quick HTML mockup of a simple landing page for this project. Reply with a single ```html block so I can preview it.',
    tags: ['coding', 'writing'],
  },
  {
    id: 'review-commit',
    title: 'Review my last commit',
    description: 'A careful second pair of eyes on your latest change.',
    prompt:
      'Review my last git commit. Point out bugs, risky changes and anything unclear, most important first.',
    tags: ['coding'],
  },
  {
    id: 'dashboard-view',
    title: 'Build me a dashboard view',
    description: 'Put sessions and terminals side by side on one screen.',
    prompt:
      'Build me a Peckboard view that works as a dashboard: show my most recent sessions side by side so I can watch them at once.',
    tags: ['ops', 'coding'],
  },
  {
    id: 'repeating-task',
    title: 'Set up a repeating task',
    description: 'Have an AI helper do a job on a schedule.',
    prompt:
      'Help me set up a repeating task. Ask me what job it should do and how often, then create it.',
    tags: ['ops', 'writing'],
  },
  {
    id: 'write-skill',
    title: 'Write a skill',
    description: 'Save instructions an AI helper reuses for one kind of job.',
    prompt:
      'Help me write a skill: ask me which job I repeat often, then save clear step-by-step instructions for it as a skill.',
    tags: ['writing', 'coding'],
  },
  {
    id: 'print-farm-status',
    title: 'Check my printers',
    description: 'See what each 3D printer is doing right now.',
    prompt: 'List my 3D printers and tell me what each one is doing, with any errors first.',
    tags: ['printing'],
    plugin: 'print-farm',
  },
  {
    id: 'pr-review',
    title: 'Review a pull request',
    description: 'Run a structured review of an open PR.',
    prompt:
      'Start a pull request review. Ask me which repository and PR number, then walk me through the findings.',
    tags: ['coding'],
    plugin: 'pr-review',
  },
  {
    id: 'ssh-fleet-health',
    title: 'Check my servers',
    description: 'Run a quick health check across your SSH hosts.',
    prompt:
      'Check the health of my SSH hosts: uptime, disk space and memory for each one. Flag anything that looks wrong.',
    tags: ['ops'],
    plugin: 'ssh-fleet',
  },
]

/** Plugins whose prompts may never be offered, whatever the catalog says. */
const NEVER_PROMPTED_PLUGINS = new Set(['homekit', 'homekit-bridge'])

/** The prompts available given which plugins are active (installed, enabled,
 *  not experimental). Plugin prompts come before the general ones they'd
 *  otherwise be cut behind. */
export function availableStarterPrompts(
  prompts: StarterPrompt[],
  activePlugins: ReadonlySet<string>,
): StarterPrompt[] {
  const fromPlugins = prompts.filter(
    (p) => p.plugin && activePlugins.has(p.plugin) && !NEVER_PROMPTED_PLUGINS.has(p.plugin),
  )
  return [...fromPlugins, ...prompts.filter((p) => !p.plugin)]
}
/**
 * Order prompts by the user's use cases: a prompt matching an earlier use
 * case ranks first; prompts matching none keep their catalog order after
 * the matches. Stable, pure, and a no-op for an empty `useCases`.
 */
export function orderStarterPrompts(
  prompts: StarterPrompt[],
  useCases: readonly string[],
): StarterPrompt[] {
  const rank = (p: StarterPrompt) => {
    let best = useCases.length
    for (const tag of p.tags) {
      const i = useCases.indexOf(tag)
      if (i !== -1 && i < best) best = i
    }
    return best
  }
  return prompts
    .map((p, i) => ({ p, i, r: rank(p) }))
    .sort((a, b) => a.r - b.r || a.i - b.i)
    .map((x) => x.p)
}
