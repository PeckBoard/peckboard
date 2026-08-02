interface PickerLoadErrorProps {
  label: string
  onRetry: () => void
}

/** Inline "failed to load" notice with a Retry affordance, for pickers whose
 *  catalogue fetch failed. Distinguishes "couldn't load" from "genuinely
 *  empty" so a transient network error doesn't read as "there is nothing
 *  here". */
export default function PickerLoadError({ label, onRetry }: PickerLoadErrorProps) {
  return (
    <p className="form-error picker-load-error" role="alert">
      Couldn&apos;t load {label}.{' '}
      <button type="button" className="form-link-btn" onClick={onRetry}>
        Retry
      </button>
    </p>
  )
}
