import type { RepeatingScheduleKind } from '../types/api'
import {
  MAX_INTERVAL_MINUTES,
  MIN_INTERVAL_MINUTES,
  scheduleProblem,
} from '../utils/repeatingSchedule'
import FieldError from './FieldError'
import TimezoneSelect from './TimezoneSelect'

interface ScheduleEditorProps {
  kind: RepeatingScheduleKind
  value: Record<string, number | string>
  onChange: (kind: RepeatingScheduleKind, value: Record<string, number | string>) => void
  /** IANA zone name, or '' for UTC. Ignored for `interval` (pure duration
   *  math, timezone-independent). */
  timezone: string
  onTimezoneChange: (tz: string) => void
}

const WEEKDAYS = ['Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday', 'Sunday']

function defaultValueFor(kind: RepeatingScheduleKind): Record<string, number | string> {
  switch (kind) {
    case 'interval':
      return { minutes: 60 }
    case 'daily':
      return { hour: 9, minute: 0 }
    case 'weekly':
      return { weekday: 0, hour: 9, minute: 0 }
    case 'monthly':
      return { day: 1, hour: 9, minute: 0 }
    case 'once':
      return { at: '' }
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
  timezone,
  onTimezoneChange,
}: ScheduleEditorProps) {
  // NOT clamped to the minimum: an out-of-range interval stays visible and
  // is reported by `scheduleProblem` instead of being silently rewritten.
  const minutes = typeof value.minutes === 'number' ? value.minutes : Number(value.minutes ?? 60)
  const intervalProblem = scheduleProblem(kind, value)
  const hour = clampInt(Number(value.hour ?? 9), 0, 23)
  const minute = clampInt(Number(value.minute ?? 0), 0, 59)
  const weekday = clampInt(Number(value.weekday ?? 0), 0, 6)
  const day = clampInt(Number(value.day ?? 1), 1, 31)
  const at = typeof value.at === 'string' ? value.at : ''

  return (
    <>
      <div className="form-field">
        <label className="form-label" htmlFor="schedule-kind">
          Schedule
        </label>
        <select
          id="schedule-kind"
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
          <option value="monthly">Monthly on a specific day</option>
          <option value="once">Run once at a specific time</option>
        </select>
      </div>

      {kind === 'interval' && (
        <div className="form-field">
          <label className="form-label" htmlFor="schedule-minutes">
            Every (minutes)
          </label>
          <input
            id="schedule-minutes"
            type="number"
            className="form-input"
            value={Number.isFinite(minutes) ? minutes : ''}
            min={MIN_INTERVAL_MINUTES}
            max={MAX_INTERVAL_MINUTES}
            aria-invalid={intervalProblem ? true : undefined}
            onChange={(e) => {
              const parsed = parseInt(e.target.value, 10)
              onChange('interval', {
                minutes: Number.isFinite(parsed)
                  ? Math.min(Math.trunc(parsed), MAX_INTERVAL_MINUTES)
                  : Number.NaN,
              })
            }}
          />
          <FieldError message={intervalProblem} testId="schedule-minutes-error" />
          <p className="form-help">
            Minimum 1 minute. The first run fires roughly this far from now; subsequent runs advance
            from the moment each run started.
          </p>
        </div>
      )}

      {kind === 'daily' && (
        <div className="form-field-row">
          <div className="form-field">
            <label className="form-label" htmlFor="schedule-hour">
              Hour
            </label>
            <input
              id="schedule-hour"
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
            <label className="form-label" htmlFor="schedule-minute">
              Minute
            </label>
            <input
              id="schedule-minute"
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
            <label className="form-label" htmlFor="schedule-weekday">
              Day of week
            </label>
            <select
              id="schedule-weekday"
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
              <label className="form-label" htmlFor="schedule-hour">
                Hour
              </label>
              <input
                id="schedule-hour"
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
              <label className="form-label" htmlFor="schedule-minute">
                Minute
              </label>
              <input
                id="schedule-minute"
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

      {kind === 'monthly' && (
        <>
          <div className="form-field">
            <label className="form-label" htmlFor="schedule-day">
              Day of month
            </label>
            <select
              id="schedule-day"
              className="form-input"
              value={day}
              onChange={(e) =>
                onChange('monthly', { day: parseInt(e.target.value, 10), hour, minute })
              }
            >
              {Array.from({ length: 31 }, (_, i) => i + 1).map((d) => (
                <option key={d} value={d}>
                  {d}
                </option>
              ))}
            </select>
            <p className="form-help">
              Clamped to the last day of shorter months (e.g. day 31 in February runs on the 28th).
            </p>
          </div>
          <div className="form-field-row">
            <div className="form-field">
              <label className="form-label" htmlFor="schedule-hour">
                Hour
              </label>
              <input
                id="schedule-hour"
                type="number"
                className="form-input"
                value={hour}
                min={0}
                max={23}
                onChange={(e) =>
                  onChange('monthly', {
                    day,
                    hour: clampInt(parseInt(e.target.value, 10), 0, 23),
                    minute,
                  })
                }
              />
            </div>
            <div className="form-field">
              <label className="form-label" htmlFor="schedule-minute">
                Minute
              </label>
              <input
                id="schedule-minute"
                type="number"
                className="form-input"
                value={minute}
                min={0}
                max={59}
                onChange={(e) =>
                  onChange('monthly', {
                    day,
                    hour,
                    minute: clampInt(parseInt(e.target.value, 10), 0, 59),
                  })
                }
              />
            </div>
          </div>
        </>
      )}

      {kind === 'once' && (
        <div className="form-field">
          <label className="form-label" htmlFor="schedule-at">
            Date &amp; time
          </label>
          <input
            id="schedule-at"
            type="datetime-local"
            className="form-input"
            value={at}
            aria-invalid={intervalProblem ? true : undefined}
            onChange={(e) => onChange('once', { at: e.target.value })}
          />
          <FieldError message={intervalProblem} testId="schedule-at-error" />
          <p className="form-help">
            Fires once at this wall-clock time, then the task disables itself automatically.
          </p>
        </div>
      )}

      {kind !== 'interval' && (
        <div className="form-field">
          <label className="form-label" htmlFor="schedule-timezone">
            Timezone
          </label>
          <TimezoneSelect id="schedule-timezone" value={timezone} onChange={onTimezoneChange} />
          <p className="form-help">Times above are interpreted in this zone and survive DST.</p>
        </div>
      )}
    </>
  )
}
