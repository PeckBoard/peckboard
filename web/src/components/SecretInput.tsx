import { useState } from 'react'

interface SecretInputProps {
  value: string
  onChange: (value: string) => void
  placeholder?: string
  /** Extra classes for the <input> itself (the wrapper keeps its own). */
  className?: string
  /** id for the <input>, so a sibling <label htmlFor> can address it. */
  id?: string
  /** What the toggle announces, e.g. "value" → "Reveal value". */
  label?: string
  testId?: string
  revealTestId?: string
  disabled?: boolean
  /** Render a <textarea> instead of an <input> — for secrets that are
   *  genuinely multi-line, like a pasted private-key PEM. */
  multiline?: boolean
  /** Rows for the multiline variant (ignored otherwise). */
  rows?: number
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
  id,
  label = 'value',
  testId,
  revealTestId,
  disabled,
  multiline,
  rows = 6,
}: SecretInputProps) {
  const [shown, setShown] = useState(false)
  return (
    <span
      className={
        multiline
          ? `secret-input secret-input--multiline${shown ? '' : ' secret-input--masked'}`
          : 'secret-input'
      }
    >
      {multiline ? (
        // A textarea can't take `type="password"`, so the masking lives in
        // CSS (`.secret-input--masked`) and the toggle flips that class.
        <textarea
          id={id}
          className={className}
          value={value}
          rows={rows}
          placeholder={placeholder}
          autoComplete="off"
          spellCheck={false}
          disabled={disabled}
          data-testid={testId}
          onChange={(e) => onChange(e.target.value)}
        />
      ) : (
        <input
          id={id}
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
      )}
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
