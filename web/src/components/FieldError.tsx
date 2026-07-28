interface Props {
  /** The unmet requirement, in plain words. Empty/undefined renders nothing. */
  message?: string
  /** Stable hook for Playwright, e.g. `new-user-password-error`. */
  testId?: string
}

/**
 * Inline validation message anchored to a single field.
 *
 * Use this instead of appending to a form-wide error banner: a banner that
 * lists constraints can end up naming a field the user filled in correctly,
 * which is the defect this component exists to prevent.
 */
export default function FieldError({ message, testId }: Props) {
  if (!message) return null
  return (
    <p className="form-field-error" data-testid={testId} role="alert">
      {message}
    </p>
  )
}
