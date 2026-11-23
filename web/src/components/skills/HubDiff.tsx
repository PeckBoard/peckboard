import DiffBlock from '../DiffBlock'

/** A unified diff of a skill's SKILL.md from Peckboard Public (merge
 *  requests, history), shown open in the shared `DiffBlock`. */
export default function HubDiff({ diff, testId }: { diff: string; testId?: string }) {
  const lines = diff.split('\n')
  const isBody = (l: string, mark: string) => l.startsWith(mark) && !l.startsWith(mark.repeat(3))
  const added = lines.filter((l) => isBody(l, '+')).length
  const removed = lines.filter((l) => isBody(l, '-')).length
  if (!diff.trim()) {
    return (
      <p className="form-hint" data-testid={testId}>
        No text changed.
      </p>
    )
  }
  return (
    <div data-testid={testId}>
      <DiffBlock
        diff={{ path: 'SKILL.md', diff: diff.replace(/\n$/, ''), added, removed }}
        defaultExpanded
      />
    </div>
  )
}
