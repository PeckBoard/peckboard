import { useState } from 'react'

interface SecretInputProps {
  value: string
  onChange: (value: string) => void
  placeholder?: string
  /** Extra classes for the <input> itself (the wrapper keeps its own). */
  className?: string
  /** What the toggle announces, e.g. "value" → "Reveal value". */
  label?: string
  testId?: string
  revealTestId?: string
  disabled?: boolean
}

/**
 * A text field for a secret: masked by default, with a Reveal/Hide toggle
 * that matches the one on the environment-variable list rows so entering a
 * secret and reading one back are the same interaction.
 *
 * The value never reaches a `title` attribute, a test id or the console —
 * the only way to see it is the toggle.
 */
export default function SecretInput({
  value,
  onChange,
  placeholder,
  className,
  label = 'value',
  testId,
  revealTestId,
  disabled,
}: SecretInputProps) {
  const [shown, setShown] = useState(false)
  return (
    <span className="secret-input">
      <input
        className={className}
        type={shown ? 'text' : 'password'}
        value={value}
        placeholder={placeholder}
        autoComplete="off"
        spellCheck={false}
        disabled={disabled}
        data-testid={testId}
        onChange={(e) => onChange(e.target.value)}
      />
      <button
        type="button"
        className="secret-input-toggle"
        aria-pressed={shown}
        aria-label={shown ? `Hide ${label}` : `Reveal ${label}`}
        disabled={disabled}
        data-testid={revealTestId}
        onClick={() => setShown((s) => !s)}
      >
        {shown ? 'Hide' : 'Reveal'}
      </button>
    </span>
  )
}
