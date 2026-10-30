import VoiceDock from '../VoiceDock'

interface AssistantTranscriptDrawerProps {
  open: boolean
}

export default function AssistantTranscriptDrawer({ open }: AssistantTranscriptDrawerProps) {
  return (
    <aside
      className="assistant-transcript-drawer"
      data-testid="assistant-transcript"
      data-open={open ? 'true' : 'false'}
      aria-label="Assistant transcript"
      aria-hidden={!open}
      inert={open ? undefined : true}
    >
      <VoiceDock inputTestId="assistant-text-input" />
    </aside>
  )
}
