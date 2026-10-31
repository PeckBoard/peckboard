import { useEffect, useMemo, useState } from 'react'
import { useAssistantActivity } from '../../store/assistantActivity'
import { useAssistantTopics } from '../../store/assistantTopics'
import { useRepeatingTasksStore } from '../../store/repeatingTasks'
import { useResourcesStore } from '../../store/resources'
import { useVoiceStore } from '../../store/voice'
import type { OrbState } from '../../voice/assistantContract'
import { audioLevels, useOrbState } from '../../voice/levels'
import '../../styles/assistant.css'
import ModelPicker from '../ModelPicker'
import AssistantActivity from './AssistantActivity'
import AssistantCaptions from './AssistantCaptions'
import AssistantOrb from './AssistantOrb'
import AssistantEmbedCard from './AssistantEmbedCard'
import AssistantTranscriptDrawer from './AssistantTranscriptDrawer'
import AssistantUpcoming from './AssistantUpcoming'
import TopicRing from './TopicRing'
import { upcomingAssistantTasks } from './upcoming'
import { useAbsorbAnimation } from './useAbsorbAnimation'

const STATUS_LABEL: Record<OrbState, string> = {
  offline: 'Offline',
  idle: 'Ready',
  listening: 'Listening',
  thinking: 'Thinking',
  speaking: 'Speaking',
}

function MicIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden>
      <rect x="9" y="3" width="6" height="11" rx="3" />
      <path d="M5 11a7 7 0 0 0 14 0M12 18v3M8 21h8" />
    </svg>
  )
}

function TranscriptIcon() {
  return (
    <svg viewBox="0 0 24 24" aria-hidden>
      <path d="M4 5h16M4 10h16M4 15h10M4 20h7" />
    </svg>
  )
}

export default function AssistantPage() {
  const topics = useAssistantTopics((state) => state.topics)
  const currentId = useAssistantTopics((state) => state.currentId)
  const loading = useAssistantTopics((state) => state.loading)
  const error = useAssistantTopics((state) => state.error)
  const orbState = useOrbState()

  const recognitionSupported = useVoiceStore((state) => state.recognitionSupported)
  const panelOpen = useVoiceStore((state) => state.panelOpen)
  const micOn = useVoiceStore((state) => state.micOn)
  const openPanel = useVoiceStore((state) => state.openPanel)
  const toggleMic = useVoiceStore((state) => state.toggleMic)
  const unlockSpeech = useVoiceStore((state) => state.unlockSpeech)
  const [transcriptOpen, setTranscriptOpen] = useState(false)
  const [upcomingOpen, setUpcomingOpen] = useState(false)
  const model = useVoiceStore((state) => state.model)
  const setModel = useVoiceStore((state) => state.setModel)
  const models = useResourcesStore((state) => state.models)
  const fetchModels = useResourcesStore((state) => state.fetchModels)
  const tasks = useRepeatingTasksStore((state) => state.tasks)
  const upcomingCount = useMemo(() => upcomingAssistantTasks(tasks).length, [tasks])

  const { absorbing, displayedId, durationMs } = useAbsorbAnimation(topics, currentId)
  const displayedTopic = useMemo(
    () => topics.find((topic) => topic.id === displayedId) ?? null,
    [displayedId, topics],
  )

  useEffect(() => {
    void useAssistantTopics.getState().load()
    void useVoiceStore.getState().openPanel()
    void useRepeatingTasksStore
      .getState()
      .fetchTasks()
      .catch(() => undefined)
    return useAssistantActivity.getState().start()
  }, [])

  useEffect(() => {
    if (models.length === 0) void fetchModels()
  }, [models.length, fetchModels])

  const handleListen = () => {
    if (!panelOpen) {
      void openPanel()
      return
    }
    toggleMic()
  }

  return (
    <main
      className="assistant-page"
      data-testid="assistant-page"
      data-state={orbState}
      data-transcript-open={transcriptOpen ? 'true' : 'false'}
      data-upcoming-open={upcomingOpen ? 'true' : 'false'}
      onPointerDown={unlockSpeech}
    >
      <div className="assistant-ambient assistant-ambient-one" aria-hidden />
      <div className="assistant-ambient assistant-ambient-two" aria-hidden />

      <div className="assistant-status">
        <span
          className="assistant-status-dot"
          data-testid="assistant-status-dot"
          data-state={orbState}
          aria-hidden
        />
        <span>{STATUS_LABEL[orbState]}</span>
      </div>

      <AssistantActivity />

      <div className="assistant-toolbar">
        <ModelPicker
          value={model ?? ''}
          onChange={(id) => {
            if (id && id !== model) void setModel(id)
          }}
          models={models}
          ariaLabel="Assistant model"
          testId="assistant-model"
          triggerClassName="assistant-chip assistant-chip-button assistant-model-trigger"
          align="right"
          emptyHint="Loading models…"
          onOpen={fetchModels}
          valueLabel={model === null ? 'Loading…' : undefined}
        />
        <button
          type="button"
          className="assistant-chip assistant-chip-button"
          data-testid="assistant-upcoming-toggle"
          aria-expanded={upcomingOpen}
          onClick={() => setUpcomingOpen((o) => !o)}
        >
          Upcoming
          {upcomingCount > 0 && <span className="assistant-chip-count">{upcomingCount}</span>}
        </button>
      </div>

      <TopicRing
        topics={topics}
        currentId={currentId}
        absorbing={absorbing}
        absorbDurationMs={durationMs}
        onFocus={(id) => void useAssistantTopics.getState().focus(id)}
      >
        <AssistantOrb
          state={orbState}
          audioLevels={audioLevels}
          currentTopic={displayedTopic}
          expanded={currentId !== null}
          absorbing={absorbing !== null}
        />
      </TopicRing>

      {loading && topics.length === 0 && (
        <div className="assistant-page-notice" role="status">
          Gathering topics…
        </div>
      )}
      {error && (
        <div className="assistant-page-notice assistant-page-error" role="alert">
          {error}
        </div>
      )}
      <AssistantCaptions />
      <AssistantEmbedCard />
      <button
        type="button"
        className="assistant-corner-button assistant-listen"
        data-testid="assistant-listen"
        data-active={micOn ? 'true' : 'false'}
        aria-label={micOn ? 'Mute microphone' : 'Start listening'}
        aria-pressed={micOn}
        disabled={!recognitionSupported}
        title={
          recognitionSupported
            ? micOn
              ? 'Mute microphone'
              : 'Start listening'
            : 'Speech recognition is not available in this browser'
        }
        onClick={handleListen}
      >
        <MicIcon />
        <span>{micOn ? 'Listening' : 'Listen'}</span>
      </button>

      <button
        type="button"
        className="assistant-corner-button assistant-transcript-toggle"
        data-testid="assistant-transcript-toggle"
        aria-label={transcriptOpen ? 'Close transcript' : 'Open transcript'}
        aria-expanded={transcriptOpen}
        onClick={() => setTranscriptOpen((open) => !open)}
      >
        <TranscriptIcon />
        <span>Transcript</span>
      </button>

      <AssistantTranscriptDrawer open={transcriptOpen} />
      <AssistantUpcoming open={upcomingOpen} onClose={() => setUpcomingOpen(false)} />
    </main>
  )
}
