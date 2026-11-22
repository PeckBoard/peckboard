import { useState } from 'react'
import ListViewHeader from './ListViewHeader'
import DemoProjectCard from './DemoProjectCard'
import Modal from './Modal'
import PhonePairingCard from './PhonePairingCard'
import NewFolderModal from './NewFolderModal'
import { useGettingStartedSteps } from '../hooks/useGettingStartedSteps'
import { orderByUseCases, useUseCases } from '../store/useCases'
import { WELCOME_CAPABILITIES, type WelcomeAction, type WelcomeCard } from './welcomeCards'
import { DOCS_BASE } from '../utils/helpTerms'
import '../styles/welcome.css'

interface Props {
  onAction: (action: WelcomeAction) => void
  /**
   * EXTENSION SLOT — cards the host adds to the top of the gallery, ahead of
   * the built-in capabilities: e.g. "See a demo project" and "Use PeckBoard
   * on your phone". They are ordered by the user's use cases among
   * themselves, like the built-ins.
   */
  extraCards?: WelcomeCard[]
}

/**
 * The Welcome page (`/welcome`): a walkthrough of the getting-started steps
 * (ticked from real data, shared with the Sessions-page summary via
 * `useGettingStartedSteps`), each with a short explanation, and a gallery of
 * what PeckBoard can do, each card starting the matching flow. The gallery
 * puts the cards that match the user's use cases (setup wizard) first.
 */
export default function WelcomePage({ onAction, extraCards = [] }: Props) {
  const [showNewFolder, setShowNewFolder] = useState(false)
  const [showPhone, setShowPhone] = useState(false)
  const useCases = useUseCases()
  const { steps, doneCount, allDone } = useGettingStartedSteps({
    onConnectAi: () => onAction('connectAi'),
    onAddFolder: () => setShowNewFolder(true),
    onNewSession: () => onAction('newSession'),
    onNewProject: () => onAction('newProject'),
    onOpenProjects: () => onAction('openProjects'),
  })
  const [pickedKey, setPickedKey] = useState<string | null>(null)
  const selected = steps.find((s) => s.key === pickedKey) ?? steps.find((s) => !s.done) ?? steps[0]

  const builtIn: WelcomeCard[] = WELCOME_CAPABILITIES.map(({ action, ...card }) => ({
    ...card,
    onTry: () => onAction(action),
  }))
  const phoneCard: WelcomeCard = {
    id: 'phone',
    title: 'Use it on your phone',
    body: 'Scan a code to open PeckBoard on your phone and keep an eye on your helpers anywhere.',
    tryLabel: 'Show code',
    useCases: ['ops', 'printing', 'other'],
    onTry: () => setShowPhone(true),
  }
  const cards = [
    ...orderByUseCases(extraCards, (c) => c.useCases, useCases),
    ...orderByUseCases([...builtIn, phoneCard], (c) => c.useCases, useCases),
  ]

  return (
    <div className="list-view welcome-page" data-testid="welcome-page">
      <ListViewHeader title="Welcome" />
      <div className="welcome-body">
        <p className="welcome-lead">
          PeckBoard gives you AI helpers that do real work on this computer. Start with the steps
          below, then try whatever looks useful.{' '}
          <a
            className="form-link"
            href={`${DOCS_BASE}/getting-started.html`}
            target="_blank"
            rel="noreferrer noopener"
          >
            Learn more ↗
          </a>
        </p>

        <section className="welcome-section" aria-labelledby="welcome-walkthrough-title">
          <div className="welcome-section-head">
            <h2 id="welcome-walkthrough-title">Walkthrough</h2>
            <span className="getting-started-progress" data-testid="welcome-progress">
              {allDone ? 'All done' : `${doneCount} of ${steps.length} done`}
            </span>
          </div>
          <div className="welcome-walkthrough">
            <ol className="welcome-steps" aria-label="Steps">
              {steps.map((step, i) => (
                <li key={step.key}>
                  <button
                    type="button"
                    className={`welcome-step${step.done ? ' done' : ''}${step.key === selected?.key ? ' selected' : ''}`}
                    aria-pressed={step.key === selected?.key}
                    onClick={() => setPickedKey(step.key)}
                    data-testid={`welcome-step-${step.key}`}
                    data-done={step.done ? 'true' : 'false'}
                  >
                    <span className="welcome-step-dot" aria-hidden="true">
                      {step.done ? '✓' : i + 1}
                    </span>
                    <span className="welcome-step-label">{step.label}</span>
                    <span className="sr-only">{step.done ? ' (done)' : ' (to do)'}</span>
                  </button>
                </li>
              ))}
            </ol>
            {selected && (
              <div
                className="welcome-step-panel"
                aria-live="polite"
                data-testid="welcome-step-panel"
              >
                <h3>{selected.label}</h3>
                <p>{selected.detail}</p>
                {selected.done ? (
                  <p className="welcome-step-done">✓ Done</p>
                ) : (
                  selected.actionLabel &&
                  selected.onAction && (
                    <button
                      type="button"
                      className="btn-primary"
                      onClick={selected.onAction}
                      data-testid="welcome-step-action"
                    >
                      {selected.actionLabel}
                    </button>
                  )
                )}
              </div>
            )}
          </div>
        </section>

        <section className="welcome-section welcome-demo" aria-label="Demo project">
          <DemoProjectCard />
        </section>

        <section className="welcome-section" aria-labelledby="welcome-gallery-title">
          <div className="welcome-section-head">
            <h2 id="welcome-gallery-title">What PeckBoard can do</h2>
          </div>
          <ul className="welcome-gallery" data-testid="welcome-gallery">
            {cards.map((card) => (
              <li key={card.id} className="welcome-card" data-testid={`welcome-card-${card.id}`}>
                <h3>{card.title}</h3>
                <p>{card.body}</p>
                <button
                  type="button"
                  className="btn-secondary"
                  onClick={card.onTry}
                  data-testid={`welcome-card-${card.id}-try`}
                >
                  {card.tryLabel ?? 'Try it'}
                </button>
              </li>
            ))}
          </ul>
        </section>
      </div>
      {showNewFolder && <NewFolderModal onClose={() => setShowNewFolder(false)} />}
      {showPhone && (
        <Modal onClose={() => setShowPhone(false)} maxWidth={420} data-testid="welcome-phone-modal">
          <h2>Use PeckBoard on your phone</h2>
          <PhonePairingCard showTitle={false} />
          <div className="form-actions">
            <button type="button" className="btn-secondary" onClick={() => setShowPhone(false)}>
              Close
            </button>
          </div>
        </Modal>
      )}
    </div>
  )
}
