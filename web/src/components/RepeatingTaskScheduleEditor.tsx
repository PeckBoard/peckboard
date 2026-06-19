import type { RepeatingScheduleKind } from '../types/api'

interface ScheduleEditorProps {
  kind: RepeatingScheduleKind
  value: Record<string, number>
  onChange: (kind: RepeatingScheduleKind, value: Record<string, number>) => void
}

const WEEKDAYS = ['Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday', 'Sunday']

function defaultValueFor(kind: RepeatingScheduleKind): Record<string, number> {
  switch (kind) {
    case 'interval':
      return { minutes: 60 }
    case 'daily':
      return { hour: 9, minute: 0 }
    case 'weekly':
      return { weekday: 0, hour: 9, minute: 0 }
  }
}

function clampInt(n: number, min: number, max: number): number {
  if (!Number.isFinite(n)) return min
  return Math.max(min, Math.min(max, Math.trunc(n)))
}

export default function RepeatingTaskScheduleEditor({
  kind,
  value,
  onChange,
}: ScheduleEditorProps) {
  const minutes = clampInt(value.minutes ?? 60, 1, 525600)
  const hour = clampInt(value.hour ?? 9, 0, 23)
  const minute = clampInt(value.minute ?? 0, 0, 59)
  const weekday = clampInt(value.weekday ?? 0, 0, 6)

  return (
    <>
      <div className="form-field">
        <label className="form-label">Schedule</label>
        <select
          className="form-input"
          value={kind}
          onChange={(e) => {
            const nextKind = e.target.value as RepeatingScheduleKind
            onChange(nextKind, defaultValueFor(nextKind))
          }}
        >
          <option value="interval">Every N minutes</option>
          <option value="daily">Daily at a specific time</option>
          <option value="weekly">Weekly on a specific day</option>
        </select>
      </div>

      {kind === 'interval' && (
        <div className="form-field">
          <label className="form-label">Every (minutes)</label>
          <input
            type="number"
            className="form-input"
            value={minutes}
            min={1}
            max={525600}
            onChange={(e) =>
              onChange('interval', { minutes: clampInt(parseInt(e.target.value, 10), 1, 525600) })
            }
          />
          <p className="form-help">
            Minimum 1 minute. The first run fires roughly this far from now; subsequent runs advance
            from the moment each run started.
          </p>
        </div>
      )}

      {kind === 'daily' && (
        <div className="form-field-row">
          <div className="form-field">
            <label className="form-label">Hour (UTC)</label>
            <input
              type="number"
              className="form-input"
              value={hour}
              min={0}
              max={23}
              onChange={(e) =>
                onChange('daily', {
                  hour: clampInt(parseInt(e.target.value, 10), 0, 23),
                  minute,
                })
              }
            />
          </div>
          <div className="form-field">
            <label className="form-label">Minute</label>
            <input
              type="number"
              className="form-input"
              value={minute}
              min={0}
              max={59}
              onChange={(e) =>
                onChange('daily', {
                  hour,
                  minute: clampInt(parseInt(e.target.value, 10), 0, 59),
                })
              }
            />
          </div>
        </div>
      )}

      {kind === 'weekly' && (
        <>
          <div className="form-field">
            <label className="form-label">Day of week</label>
            <select
              className="form-input"
              value={weekday}
              onChange={(e) =>
                onChange('weekly', { weekday: parseInt(e.target.value, 10), hour, minute })
              }
            >
              {WEEKDAYS.map((d, i) => (
                <option key={d} value={i}>
                  {d}
                </option>
              ))}
            </select>
          </div>
          <div className="form-field-row">
            <div className="form-field">
              <label className="form-label">Hour (UTC)</label>
              <input
                type="number"
                className="form-input"
                value={hour}
                min={0}
                max={23}
                onChange={(e) =>
                  onChange('weekly', {
                    weekday,
                    hour: clampInt(parseInt(e.target.value, 10), 0, 23),
                    minute,
                  })
                }
              />
            </div>
            <div className="form-field">
              <label className="form-label">Minute</label>
              <input
                type="number"
                className="form-input"
                value={minute}
                min={0}
                max={59}
                onChange={(e) =>
                  onChange('weekly', {
                    weekday,
                    hour,
                    minute: clampInt(parseInt(e.target.value, 10), 0, 59),
                  })
                }
              />
            </div>
          </div>
        </>
      )}
    </>
  )
}
