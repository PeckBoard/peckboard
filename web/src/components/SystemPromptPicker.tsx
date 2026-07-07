import { useEffect } from 'react'
import { useResourcesStore } from '../store/resources'
import ModelPicker from './ModelPicker'

interface SystemPromptPickerProps {
  value: string | null
  onChange: (name: string | null) => void
  testId?: string
  disabled?: boolean
}

/**
 * Searchable picker for named system prompts. Wraps ModelPicker by mapping
 * each prompt to the {id, display_name} shape it expects. The empty string
 * represents "(none)" — no system prompt override.
 */
export default function SystemPromptPicker({
  value,
  onChange,
  testId,
  disabled,
}: SystemPromptPickerProps) {
  const systemPrompts = useResourcesStore((s) => s.systemPrompts)
  const fetchSystemPrompts = useResourcesStore((s) => s.fetchSystemPrompts)

  useEffect(() => {
    fetchSystemPrompts()
  }, [fetchSystemPrompts])

  // Map prompts to the ModelInfo shape ModelPicker expects.
  const models = systemPrompts.map((p) => ({ id: p.name, display_name: p.name }))

  return (
    <ModelPicker
      value={value ?? ''}
      onChange={(id) => onChange(id || null)}
      models={models}
      defaultLabel="(none)"
      ariaLabel="Select system prompt"
      emptyHint="Loading system prompts…"
      testId={testId}
      disabled={disabled}
    />
  )
}
