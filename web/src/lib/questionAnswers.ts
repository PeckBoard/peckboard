/**
 * Selection state for `ask_user` question cards.
 *
 * A single-select or free-text answer is a plain string; a multi-select
 * answer is a `string[]` of the exact option labels. Selections used to be
 * kept as one comma-joined string and recovered with `split(',')`, which
 * corrupted any agent-supplied label containing a comma ("Yes, restart it"
 * would never read back as selected). The array form keeps labels intact;
 * joining happens only once, at submit time, purely for display/transport.
 */
export type AnswerValue = string | string[]

/** The multi-select labels currently picked (empty for text/radio answers). */
export function selectedOptions(value: AnswerValue | undefined): string[] {
  return Array.isArray(value) ? value : []
}

/** Add the option if absent, remove it if present. */
export function toggleOption(value: AnswerValue | undefined, option: string): string[] {
  const selected = selectedOptions(value)
  return selected.includes(option) ? selected.filter((s) => s !== option) : [...selected, option]
}

/** The submitted answer text: labels joined for multi-select, trimmed otherwise. */
export function answerText(value: AnswerValue | undefined): string {
  if (Array.isArray(value)) return value.join(', ')
  return (value ?? '').trim()
}

/** The answer as parts: each picked label for multi-select, else the one string. */
export function answerParts(value: AnswerValue | undefined): string[] {
  if (Array.isArray(value)) return value
  const text = (value ?? '').trim()
  return text ? [text] : []
}
