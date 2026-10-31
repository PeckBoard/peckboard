import {
  useEffect,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent,
  type RefObject,
  type SyntheticEvent,
} from 'react'
import { useMediaElementPlayback } from '../media/playback'

/** Bars in a waveform; the mask scales to the player's width. */
const WAVE_BARS = 160
const WAVE_HEIGHT = 48
const SPEEDS = [0.5, 0.75, 1, 1.25, 1.5, 1.75, 2]
/** Arrow-key seek step, seconds. */
const SEEK_STEP = 5

/** Normalised peaks (0..1) per waveform key: decoded once per file. */
const peaksCache = new Map<string, Promise<number[] | null>>()

async function decodePeaks(src: string): Promise<number[] | null> {
  const res = await fetch(src)
  if (!res.ok) throw new Error(`HTTP ${res.status}`)
  const bytes = await res.arrayBuffer()
  const Ctx =
    window.OfflineAudioContext ??
    (window as unknown as { webkitOfflineAudioContext?: typeof OfflineAudioContext })
      .webkitOfflineAudioContext
  if (!Ctx) return null
  const audio = await new Ctx(1, 1, 44100).decodeAudioData(bytes)
  const channels = Array.from({ length: audio.numberOfChannels }, (_, i) => audio.getChannelData(i))
  const len = audio.length
  const bucket = Math.max(1, Math.floor(len / WAVE_BARS))
  // Sampling ~512 points per bar keeps a long track's scan cheap.
  const stride = Math.max(1, Math.floor(bucket / 512))
  const peaks: number[] = []
  let max = 0
  for (let b = 0; b < WAVE_BARS; b++) {
    let peak = 0
    const end = Math.min(len, (b + 1) * bucket)
    for (let i = b * bucket; i < end; i += stride) {
      for (const ch of channels) {
        const v = Math.abs(ch[i])
        if (v > peak) peak = v
      }
    }
    peaks.push(peak)
    if (peak > max) max = peak
  }
  return peaks.map((p) => (max > 0 ? p / max : 0))
}

function loadPeaks(key: string, src: string): Promise<number[] | null> {
  let p = peaksCache.get(key)
  if (!p) {
    p = decodePeaks(src).catch(() => null)
    peaksCache.set(key, p)
  }
  return p
}

/** Draw the bars once, opaque on transparent, as a CSS mask: the base and
 *  played layers colour it from theme variables, so light / dark switches
 *  need no redraw. */
function peaksToMask(peaks: number[]): string {
  const canvas = document.createElement('canvas')
  canvas.width = WAVE_BARS * 3
  canvas.height = WAVE_HEIGHT
  const ctx = canvas.getContext('2d')
  if (!ctx) return ''
  ctx.fillStyle = '#000'
  peaks.forEach((p, i) => {
    const h = Math.max(2, Math.round(p * WAVE_HEIGHT))
    ctx.fillRect(i * 3, (WAVE_HEIGHT - h) / 2, 2, h)
  })
  return `url(${canvas.toDataURL()})`
}

function formatTime(s: number): string {
  if (!Number.isFinite(s) || s < 0) return '0:00'
  const t = Math.floor(s)
  const h = Math.floor(t / 3600)
  const m = Math.floor((t % 3600) / 60)
  const sec = String(t % 60).padStart(2, '0')
  return h > 0 ? `${h}:${String(m).padStart(2, '0')}:${sec}` : `${m}:${sec}`
}

const PlayIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
    <path d="M8 5v14l11-7z" fill="currentColor" />
  </svg>
)
const PauseIcon = () => (
  <svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true">
    <path d="M7 5h4v14H7zM13 5h4v14h-4z" fill="currentColor" />
  </svg>
)
const VolumeIcon = ({ muted }: { muted: boolean }) => (
  <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
    <path d="M4 9v6h4l5 4V5L8 9z" fill="currentColor" />
    {muted ? (
      <path d="M16 9l5 6M21 9l-5 6" stroke="currentColor" strokeWidth="2" fill="none" />
    ) : (
      <path d="M16 8.5a5 5 0 0 1 0 7" stroke="currentColor" strokeWidth="2" fill="none" />
    )}
  </svg>
)
const LoopIcon = () => (
  <svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true">
    <path
      d="M7 7h10l-2-2M17 17H7l2 2M5 13v-2a4 4 0 0 1 2-4M19 11v2a4 4 0 0 1-2 4"
      stroke="currentColor"
      strokeWidth="2"
      fill="none"
      strokeLinecap="round"
    />
  </svg>
)

/**
 * Inline audio player: play/pause, a scrubbable waveform (or a plain bar
 * when `waveformKey` is null), time, volume, speed and loop. Registered
 * with the app-wide media coordinator, so starting it pauses other media.
 * Keyboard on the scrubber: Space toggles, arrows seek ±5 s, Home / End.
 */
export default function AudioPlayer({
  src,
  label,
  waveformKey,
  audioRef: externalRef,
  onLoadedMetadata,
  onError,
}: {
  src: string
  /** Accessible name of what plays, e.g. the filename. */
  label: string
  /** Cache key for the decoded waveform; null skips it (too large). */
  waveformKey: string | null
  audioRef?: RefObject<HTMLAudioElement | null>
  onLoadedMetadata?: (e: SyntheticEvent<HTMLAudioElement>) => void
  onError?: (e: SyntheticEvent<HTMLAudioElement>) => void
}) {
  const localRef = useRef<HTMLAudioElement | null>(null)
  const ref = externalRef ?? localRef
  const rootRef = useRef<HTMLDivElement | null>(null)
  const [playing, setPlaying] = useState(false)
  const [current, setCurrent] = useState(0)
  const [duration, setDuration] = useState(0)
  const [volume, setVolume] = useState(1)
  const [muted, setMuted] = useState(false)
  const [rate, setRate] = useState(1)
  const [loop, setLoop] = useState(false)
  const [mask, setMask] = useState<string | null>(null)
  const [waveState, setWaveState] = useState<'loading' | 'ready' | 'none'>(
    waveformKey ? 'loading' : 'none',
  )
  const dragging = useRef(false)
  useMediaElementPlayback(ref, src)

  // Decode the waveform once the player scrolls into view (it downloads
  // the whole file), then reuse the cached peaks for this file.
  useEffect(() => {
    const root = rootRef.current
    if (!waveformKey || !root) return
    let cancelled = false
    const start = () => {
      loadPeaks(waveformKey, src).then((peaks) => {
        if (cancelled) return
        const m = peaks ? peaksToMask(peaks) : ''
        setMask(m || null)
        setWaveState(m ? 'ready' : 'none')
      })
    }
    if (typeof IntersectionObserver === 'undefined') {
      start()
      return () => {
        cancelled = true
      }
    }
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          io.disconnect()
          start()
        }
      },
      { rootMargin: '200px 0px' },
    )
    io.observe(root)
    return () => {
      cancelled = true
      io.disconnect()
    }
    // A re-minted `src` re-runs this, but the peaks come from the cache.
  }, [waveformKey, src])

  // Smooth progress while playing (`timeupdate` fires only ~4×/s).
  useEffect(() => {
    if (!playing) return
    let id = 0
    const tick = () => {
      const el = ref.current
      if (el && !dragging.current) setCurrent(el.currentTime)
      id = requestAnimationFrame(tick)
    }
    id = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(id)
  }, [playing, ref])

  const hasDuration = Number.isFinite(duration) && duration > 0
  const frac = hasDuration ? Math.min(1, current / duration) : 0

  const toggle = () => {
    const el = ref.current
    if (!el) return
    if (el.paused) void el.play().catch(() => {})
    else el.pause()
  }
  const seekTo = (t: number) => {
    const el = ref.current
    if (!el || !hasDuration) return
    const next = Math.max(0, Math.min(duration, t))
    el.currentTime = next
    setCurrent(next)
  }
  const seekFromPointer = (e: PointerEvent<HTMLDivElement>) => {
    const r = e.currentTarget.getBoundingClientRect()
    if (r.width > 0) seekTo(((e.clientX - r.left) / r.width) * duration)
  }
  const onScrubKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = {
      ArrowLeft: -SEEK_STEP,
      ArrowDown: -SEEK_STEP,
      ArrowRight: SEEK_STEP,
      ArrowUp: SEEK_STEP,
    }[e.key]
    if (e.key === ' ' || e.key === 'Enter') toggle()
    else if (step !== undefined) seekTo(current + step)
    else if (e.key === 'PageDown') seekTo(current - duration / 10)
    else if (e.key === 'PageUp') seekTo(current + duration / 10)
    else if (e.key === 'Home') seekTo(0)
    else if (e.key === 'End') seekTo(duration)
    else return
    e.preventDefault()
  }
  const changeRate = (r: number) => {
    const el = ref.current
    // A re-minted `src` resets playbackRate to the default: set both.
    if (el) {
      el.defaultPlaybackRate = r
      el.playbackRate = r
    }
    setRate(r)
  }
  const changeVolume = (v: number) => {
    const el = ref.current
    if (!el) return
    el.volume = v
    el.muted = v === 0
  }

  const pct = `${(frac * 100).toFixed(2)}%`
  return (
    <div
      ref={rootRef}
      className="audio-player"
      data-testid="audio-player"
      data-playing={playing ? 'true' : 'false'}
    >
      <audio
        ref={ref}
        src={src}
        preload="metadata"
        loop={loop}
        data-testid="audio-element"
        onPlay={() => setPlaying(true)}
        onPause={() => {
          setPlaying(false)
          if (ref.current) setCurrent(ref.current.currentTime)
        }}
        onEnded={() => setPlaying(false)}
        onTimeUpdate={(e) => !dragging.current && setCurrent(e.currentTarget.currentTime)}
        onDurationChange={(e) => setDuration(e.currentTarget.duration)}
        onVolumeChange={(e) => {
          setVolume(e.currentTarget.volume)
          setMuted(e.currentTarget.muted)
        }}
        onLoadedMetadata={(e) => {
          setDuration(e.currentTarget.duration)
          e.currentTarget.playbackRate = rate
          onLoadedMetadata?.(e)
        }}
        onError={onError}
      />
      <button
        type="button"
        className="audio-player-btn audio-player-play"
        onClick={toggle}
        aria-label={playing ? `Pause ${label}` : `Play ${label}`}
        data-testid="audio-play"
      >
        {playing ? <PauseIcon /> : <PlayIcon />}
      </button>
      <div className="audio-player-main">
        <div
          className={`audio-scrub${waveState === 'ready' ? ' has-wave' : ''}`}
          role="slider"
          tabIndex={0}
          aria-label={`Seek ${label}`}
          aria-valuemin={0}
          aria-valuemax={hasDuration ? Math.round(duration) : 0}
          aria-valuenow={Math.round(current)}
          aria-valuetext={`${formatTime(current)} of ${formatTime(duration)}`}
          data-testid="audio-waveform"
          data-state={waveState}
          onKeyDown={onScrubKey}
          onPointerDown={(e) => {
            e.currentTarget.setPointerCapture(e.pointerId)
            dragging.current = true
            seekFromPointer(e)
          }}
          onPointerMove={(e) => dragging.current && seekFromPointer(e)}
          onPointerUp={() => {
            dragging.current = false
          }}
          onPointerCancel={() => {
            dragging.current = false
          }}
        >
          {waveState === 'ready' && mask ? (
            <>
              <span
                className="audio-wave audio-wave-base"
                style={{ maskImage: mask, WebkitMaskImage: mask }}
              />
              <span
                className="audio-wave audio-wave-played"
                style={{
                  maskImage: mask,
                  WebkitMaskImage: mask,
                  clipPath: `inset(0 ${(100 - frac * 100).toFixed(2)}% 0 0)`,
                }}
              />
            </>
          ) : (
            <span className="audio-bar">
              <span className="audio-bar-fill" style={{ width: pct }} />
            </span>
          )}
        </div>
        <div className="audio-player-controls">
          <span className="audio-player-time" data-testid="audio-time">
            {formatTime(current)} / {formatTime(duration)}
          </span>
          <span className="audio-player-spacer" />
          <button
            type="button"
            className="audio-player-btn"
            onClick={() => {
              const el = ref.current
              if (!el) return
              el.muted = !el.muted
              if (!el.muted && el.volume === 0) el.volume = 1
            }}
            aria-label={muted ? 'Unmute' : 'Mute'}
            aria-pressed={muted}
          >
            <VolumeIcon muted={muted || volume === 0} />
          </button>
          <input
            type="range"
            className="audio-player-volume"
            min={0}
            max={1}
            step={0.05}
            value={muted ? 0 : volume}
            onChange={(e) => changeVolume(Number(e.target.value))}
            aria-label="Volume"
          />
          <select
            className="audio-player-rate"
            value={rate}
            onChange={(e) => changeRate(Number(e.target.value))}
            aria-label="Playback speed"
          >
            {SPEEDS.map((s) => (
              <option key={s} value={s}>
                {s}×
              </option>
            ))}
          </select>
          <button
            type="button"
            className={`audio-player-btn${loop ? ' is-active' : ''}`}
            onClick={() => setLoop((l) => !l)}
            aria-label="Loop"
            aria-pressed={loop}
          >
            <LoopIcon />
          </button>
        </div>
      </div>
    </div>
  )
}
