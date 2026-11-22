import type { UseCase } from '../store/useCases'

/** A flow the Welcome page can start. The host (App) owns navigation and
 *  the create dialogs, so it maps each one to the real flow. */
export type WelcomeAction =
  | 'connectAi'
  | 'newSession'
  | 'newProject'
  | 'openProjects'
  | 'views'
  | 'mockup'
  | 'skills'
  | 'assistant'
  | 'repeatingTasks'
  | 'plugins'

/** One card in the Welcome page's "What PeckBoard can do" gallery. Also the
 *  shape of `WelcomePage`'s `extraCards` slot (demo project, phone, …). */
export interface WelcomeCard {
  /** Stable id; the card's test id is `welcome-card-<id>`. */
  id: string
  title: string
  /** One plain sentence. */
  body: string
  /** Button label (default "Try it"). */
  tryLabel?: string
  /** Use cases this card matters most for — orders the gallery. */
  useCases: readonly UseCase[]
  onTry: () => void
}

/** The built-in capability cards, in default order (before use cases). */
export const WELCOME_CAPABILITIES: (Omit<WelcomeCard, 'onTry'> & { action: WelcomeAction })[] = [
  {
    id: 'chats',
    title: 'Chats',
    body: 'Talk to an AI helper that can read and change the files in your folder.',
    useCases: ['coding', 'writing', 'ops', 'other'],
    action: 'newSession',
  },
  {
    id: 'projects',
    title: 'Projects & workers',
    body: 'Turn a goal into a board of tasks that AI workers pick up and finish on their own.',
    useCases: ['coding', 'writing'],
    action: 'newProject',
  },
  {
    id: 'views',
    title: 'Views',
    body: 'Put several chats, terminals and status panels side by side on one dashboard.',
    useCases: ['ops', 'printing'],
    action: 'views',
  },
  {
    id: 'mockups',
    title: 'Mockups',
    body: 'Ask for a page or a screen and click through a working mockup next to the chat.',
    useCases: ['coding'],
    action: 'mockup',
  },
  {
    id: 'skills',
    title: 'Skills',
    body: 'Save instructions an AI helper should follow, and reuse them in any chat.',
    useCases: ['coding', 'writing'],
    action: 'skills',
  },
  {
    id: 'voice',
    title: 'Voice assistant',
    body: 'Talk to PeckBoard out loud and hear the answers spoken back.',
    useCases: ['writing', 'other'],
    action: 'assistant',
  },
  {
    id: 'repeating',
    title: 'Repeating tasks',
    body: 'Have an AI helper do a job on a schedule — every morning, every hour, every Monday.',
    useCases: ['ops', 'printing'],
    action: 'repeatingTasks',
  },
  {
    id: 'plugins',
    title: 'Plugins',
    body: 'Add new abilities, such as 3D printer control or GitHub, from the plugin registry.',
    useCases: ['printing', 'ops'],
    action: 'plugins',
  },
]
