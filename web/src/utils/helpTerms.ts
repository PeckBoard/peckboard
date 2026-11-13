/** Base of the public docs site, for "Learn more" links. */
export const DOCS_BASE = 'https://peckboard.com'

/** A docs path under {@link DOCS_BASE}, or a full URL passed through. */
export function docsUrl(path: string): string {
  return path.startsWith('http') ? path : `${DOCS_BASE}${path}`
}

/**
 * One plain-language explanation per PeckBoard concept, shared by every
 * "What's this?" tip (`HelpTip`) and empty state so a term reads the same
 * wherever it is explained. Written for someone who has never used a coding
 * agent; `docs` carries the detail for anyone who wants more.
 */
export const HELP_TERMS = {
  session: {
    title: 'Session',
    body: 'A conversation with an AI helper. It can read and change the files in its folder, run commands, and remembers everything said in the chat.',
    docs: '/core-concepts.html',
  },
  folder: {
    title: 'Folder',
    body: 'A place on this computer where PeckBoard keeps work. The AI can read and change files inside it, and nothing outside it unless you allow that.',
    docs: '/core-concepts.html',
  },
  project: {
    title: 'Project',
    body: 'A board of tasks in one folder. You add tasks as cards, and AI helpers work through them on their own, one step at a time.',
    docs: '/core-concepts.html',
  },
  card: {
    title: 'Card',
    body: 'One task on a project board, such as "Add a contact page". Start it and an AI helper picks it up and moves it across the board as it works.',
    docs: '/core-concepts.html',
  },
  workflow: {
    title: 'Workflow',
    body: 'The steps a card goes through, shown as the board columns. "Task" is the simple default: the AI does the work, then a fresh helper checks it.',
    docs: '/core-concepts.html',
  },
  worker: {
    title: 'Worker',
    body: 'An AI helper that works on one card by itself. PeckBoard starts one when a card is started and stops it when the card is done.',
    docs: '/core-concepts.html',
  },
  model: {
    title: 'Model',
    body: 'Which AI does the thinking. Bigger models are smarter but slower and use more of your plan; smaller ones are quick for simple jobs.',
    docs: '/providers.html',
  },
  effort: {
    title: 'Effort',
    body: 'How hard the AI thinks before answering. Higher is slower and more thorough; leave it on Default unless a job needs more care.',
    docs: '/providers.html',
  },
  systemPrompt: {
    title: 'Role and instructions',
    body: 'Optional standing instructions the AI follows for the whole session, such as "You are a careful reviewer". Most chats don’t need any.',
    docs: '/core-concepts.html',
  },
  skill: {
    title: 'Skill',
    body: 'A saved set of instructions for one kind of job, such as "review a pull request". Give a session a skill and its AI helper reads it whenever that job comes up.',
    docs: '/skills.html',
  },
} as const

export type HelpTerm = keyof typeof HELP_TERMS
