import { useCallback, useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { WebLinksAddon } from '@xterm/addon-web-links'
import { WebglAddon } from '@xterm/addon-webgl'
import '@xterm/xterm/css/xterm.css'
import { getToken } from '../../store/auth'
import type { TerminalStatus } from '../../store/terminals'
import { mirrorInlineStyles } from './cspStyles'
import './Terminal.css'

const FONT_KEY = 'peckboard.terminal.fontSize'
const FONT_MIN = 9
const FONT_MAX = 28
const FONT_DEFAULT = 14

function loadFontSize(): number {
  const n = Number(localStorage.getItem(FONT_KEY))
  return Number.isFinite(n) && n >= FONT_MIN && n <= FONT_MAX ? n : FONT_DEFAULT
}

const THEME = {
  background: '#0f1115',
  foreground: '#d8dee9',
  cursor: '#e5e9f0',
  cursorAccent: '#0f1115',
  selectionBackground: '#3b4252aa',
  black: '#1c1f26',
  red: '#e06c75',
  green: '#98c379',
  yellow: '#e5c07b',
  blue: '#61afef',
  magenta: '#c678dd',
  cyan: '#56b6c2',
  white: '#d8dee9',
  brightBlack: '#5c6370',
  brightRed: '#ef8a92',
  brightGreen: '#b5e08f',
  brightYellow: '#f0d49b',
  brightBlue: '#8cc7f7',
  brightMagenta: '#dc9ef0',
  brightCyan: '#7fd0da',
  brightWhite: '#ffffff',
}

/** Put an OSC 52 payload (`<targets>;<base64>`) on the clipboard. */
function copyFromOsc52(data: string) {
  const payload = data.slice(data.indexOf(';') + 1)
  if (!payload || payload === '?') return
  try {
    const bin = atob(payload)
    const text = new TextDecoder().decode(Uint8Array.from(bin, (c) => c.charCodeAt(0)))
    void navigator.clipboard?.writeText(text).catch(() => {})
  } catch {
    /* not base64 */
  }
}

interface Props {
  terminalId: string
  /** The pane is on screen: refit and take keyboard focus. */
  active: boolean
  onStatus?: (status: TerminalStatus) => void
}

/**
 * A live view of one interactive SSH shell — xterm.js over
 * `/ws/terminal/{id}`. Every keystroke goes out immediately as a binary
 * frame; PTY output is written raw. There is deliberately no command box:
 * this is the shell itself.
 *
 * Copy/paste: selecting copies — in a tmux-backed shell a drag selects in
 * tmux, which hands the text over as OSC 52 (see `copyFromOsc52`); Shift+drag
 * always makes a native selection. Ctrl+Shift+C / Ctrl+Shift+V; right-click
 * pastes (or copies an active selection). Ctrl + / Ctrl - / Ctrl 0 change
 * the font size (remembered per browser).
 */
export default function TerminalPane({ terminalId, active, onStatus }: Props) {
  const hostRef = useRef<HTMLDivElement>(null)
  const termRef = useRef<Terminal | null>(null)
  const fitRef = useRef<FitAddon | null>(null)
  const wsRef = useRef<WebSocket | null>(null)
  const onStatusRef = useRef(onStatus)
  const [status, setStatus] = useState<TerminalStatus>({
    phase: 'connecting',
    persistent: null,
    message: null,
  })
  const [socketDown, setSocketDown] = useState(false)

  useEffect(() => {
    onStatusRef.current = onStatus
  }, [onStatus])

  const sendJson = (data: unknown) => {
    const ws = wsRef.current
    if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(data))
  }

  // Fit to the pane and tell the PTY when the grid actually changed.
  const fit = useCallback(() => {
    const term = termRef.current
    const fitAddon = fitRef.current
    const el = hostRef.current
    if (!term || !fitAddon || !el || el.offsetWidth === 0 || el.offsetHeight === 0) return
    const before = `${term.cols}x${term.rows}`
    try {
      fitAddon.fit()
    } catch {
      return
    }
    if (`${term.cols}x${term.rows}` !== before) {
      sendJson({ type: 'resize', cols: term.cols, rows: term.rows })
    }
  }, [])

  useEffect(() => {
    const el = hostRef.current
    if (!el) return
    const term = new Terminal({
      fontFamily:
        'ui-monospace, "SFMono-Regular", "SF Mono", Menlo, Consolas, "DejaVu Sans Mono", "Liberation Mono", monospace',
      fontSize: loadFontSize(),
      cursorBlink: true,
      scrollback: 10000,
      macOptionIsMeta: true,
      allowProposedApi: true,
      theme: THEME,
    })
    const fitAddon = new FitAddon()
    term.loadAddon(fitAddon)
    term.loadAddon(new WebLinksAddon())
    // OSC 52 "set clipboard": tmux sends it for a drag selection (with an
    // empty target, which @xterm/addon-clipboard ignores), vim/neovim for
    // yanks. Read queries (`?`) are never answered — the remote host must
    // not be able to read the browser's clipboard.
    term.parser.registerOscHandler(52, (data) => {
      copyFromOsc52(data)
      return true
    })
    // Before `open`: xterm injects its renderer styles there.
    const stopStyleMirror = mirrorInlineStyles(el)
    term.open(el)
    // GPU renderer when available; the DOM renderer is the fallback (and
    // takes over again if the GL context is ever lost). Automated browsers
    // (`navigator.webdriver`) keep the DOM renderer so tests can read the
    // screen as text.
    if (!navigator.webdriver) {
      try {
        const webgl = new WebglAddon()
        webgl.onContextLoss(() => webgl.dispose())
        term.loadAddon(webgl)
      } catch {
        /* DOM renderer */
      }
    }
    termRef.current = term
    fitRef.current = fitAddon
    try {
      fitAddon.fit()
    } catch {
      /* not laid out yet; the ResizeObserver refits */
    }

    const encoder = new TextEncoder()
    const sendInput = (data: string | Uint8Array<ArrayBuffer>) => {
      const ws = wsRef.current
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(typeof data === 'string' ? encoder.encode(data) : data)
      }
    }
    // While the scrollback replay is being parsed, xterm answers the
    // terminal queries recorded in it (device attributes, cursor
    // position…) via onData — replies nobody asked for now, which would
    // land in the shell as typed garbage. Drop input until it's parsed.
    let replaying = false
    term.onData((d) => {
      if (!replaying) sendInput(d)
    })
    term.onBinary((d) => {
      if (replaying) return
      const bytes = new Uint8Array(d.length)
      for (let i = 0; i < d.length; i++) bytes[i] = d.charCodeAt(i) & 0xff
      sendInput(bytes)
    })

    // Copy on select (like most terminals); clipboard failures are silent.
    term.onSelectionChange(() => {
      const sel = term.getSelection()
      if (sel) void navigator.clipboard?.writeText(sel).catch(() => {})
    })
    const paste = () => {
      void navigator.clipboard
        ?.readText()
        .then((text) => {
          if (text) term.paste(text)
        })
        .catch(() => {})
    }
    const setFont = (size: number) => {
      const next = Math.max(FONT_MIN, Math.min(FONT_MAX, size))
      term.options.fontSize = next
      localStorage.setItem(FONT_KEY, String(next))
      fit()
    }
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== 'keydown') return true
      const mod = e.ctrlKey || e.metaKey
      if (mod && e.shiftKey && (e.key === 'C' || e.key === 'c')) {
        const sel = term.getSelection()
        if (sel) void navigator.clipboard?.writeText(sel).catch(() => {})
        e.preventDefault()
        return false
      }
      if (mod && e.shiftKey && (e.key === 'V' || e.key === 'v')) {
        paste()
        e.preventDefault()
        return false
      }
      if (mod && !e.shiftKey && !e.altKey && (e.key === '=' || e.key === '+')) {
        setFont((term.options.fontSize ?? FONT_DEFAULT) + 1)
        e.preventDefault()
        return false
      }
      if (mod && !e.altKey && (e.key === '-' || e.key === '_')) {
        setFont((term.options.fontSize ?? FONT_DEFAULT) - 1)
        e.preventDefault()
        return false
      }
      if (mod && !e.shiftKey && !e.altKey && e.key === '0') {
        setFont(FONT_DEFAULT)
        e.preventDefault()
        return false
      }
      return true
    })
    const onContextMenu = (e: MouseEvent) => {
      e.preventDefault()
      const sel = term.getSelection()
      if (sel) {
        void navigator.clipboard?.writeText(sel).catch(() => {})
        term.clearSelection()
      } else {
        paste()
      }
      term.focus()
    }
    el.addEventListener('contextmenu', onContextMenu)

    // ── the socket, reconnecting until unmount ──
    let disposed = false
    let attempts = 0
    let retryTimer: ReturnType<typeof setTimeout> | null = null
    const connect = () => {
      if (disposed) return
      const proto = location.protocol === 'https:' ? 'wss:' : 'ws:'
      const ws = new WebSocket(
        `${proto}//${location.host}/ws/terminal/${encodeURIComponent(terminalId)}`,
      )
      ws.binaryType = 'arraybuffer'
      wsRef.current = ws
      // Set by the server's `replay` frame: the next binary frame is the
      // scrollback snapshot, not live output.
      let replayNext = false
      ws.onopen = () => {
        ws.send(
          JSON.stringify({
            type: 'auth',
            token: getToken() ?? '',
            cols: term.cols,
            rows: term.rows,
          }),
        )
      }
      ws.onmessage = (ev) => {
        if (ev.data instanceof ArrayBuffer) {
          if (replayNext) {
            // The server replays its scrollback on every attach: start
            // from a clean screen so a reconnect doesn't double it.
            replayNext = false
            term.reset()
            replaying = true
            term.write(new Uint8Array(ev.data), () => {
              replaying = false
            })
            return
          }
          term.write(new Uint8Array(ev.data))
          return
        }
        let msg: { type?: string } & Partial<TerminalStatus>
        try {
          msg = JSON.parse(String(ev.data))
        } catch {
          return
        }
        if (msg.type === 'status') {
          attempts = 0
          setSocketDown(false)
          const s: TerminalStatus = {
            phase: msg.phase ?? 'connecting',
            persistent: msg.persistent ?? null,
            message: msg.message ?? null,
          }
          setStatus(s)
          onStatusRef.current?.(s)
        } else if (msg.type === 'replay') {
          // The server replays its scrollback on every attach: start from
          // a clean screen so a reconnect doesn't double it.
          const bytes = (msg as { bytes?: number }).bytes ?? 0
          if (bytes > 0) replayNext = true
          else term.reset()
        } else if (msg.type === 'resync') {
          ws.close()
        }
      }
      ws.onclose = (ev) => {
        if (wsRef.current === ws) wsRef.current = null
        if (disposed) return
        // 4004: the terminal was closed / isn't ours — don't hammer.
        if (ev.code === 4004) {
          const s: TerminalStatus = { phase: 'ended', persistent: null, message: 'Terminal closed' }
          setStatus(s)
          onStatusRef.current?.(s)
          return
        }
        setSocketDown(true)
        // Not live from this viewer's side until the socket is back.
        setStatus((s) => ({ ...s, phase: 'connecting', message: null }))
        const delay = Math.min(500 * 2 ** attempts, 10_000)
        attempts += 1
        retryTimer = setTimeout(connect, delay)
      }
    }
    connect()

    const ro = new ResizeObserver(() => fit())
    ro.observe(el)

    return () => {
      disposed = true
      if (retryTimer) clearTimeout(retryTimer)
      ro.disconnect()
      el.removeEventListener('contextmenu', onContextMenu)
      wsRef.current?.close()
      wsRef.current = null
      term.dispose()
      stopStyleMirror()
      termRef.current = null
      fitRef.current = null
    }
  }, [terminalId, fit])

  // Coming on screen: refit (the pane may have been resized while hidden)
  // and put the keyboard straight into the shell.
  useEffect(() => {
    if (!active) return
    const id = requestAnimationFrame(() => {
      fit()
      termRef.current?.focus()
    })
    return () => cancelAnimationFrame(id)
  }, [active, fit])

  const restart = () => {
    sendJson({ type: 'restart' })
    termRef.current?.focus()
  }

  const overlay = socketDown
    ? { text: 'Connection to Peckboard lost — reconnecting…', action: false }
    : status.phase === 'connecting'
      ? { text: 'Connecting…', action: false }
      : status.phase === 'reconnecting'
        ? { text: `Reconnecting${status.message ? ` (${status.message})` : ''}…`, action: false }
        : status.phase === 'error'
          ? { text: status.message || 'Could not connect', action: true }
          : status.phase === 'ended'
            ? {
                text: status.message === 'Terminal closed' ? status.message : 'Shell ended',
                action: status.message !== 'Terminal closed',
              }
            : null

  return (
    <div className="terminal-pane" data-testid="terminal-pane" data-phase={status.phase}>
      <div
        ref={hostRef}
        className="terminal-pane-xterm"
        onMouseDown={() => termRef.current?.focus()}
      />
      {overlay && (
        <div
          className={`terminal-pane-overlay${status.phase === 'live' ? ' subtle' : ''}`}
          data-testid="terminal-overlay"
          role="status"
        >
          <span>{overlay.text}</span>
          {overlay.action && (
            <button
              type="button"
              className="btn-primary"
              onClick={restart}
              data-testid="terminal-restart"
            >
              {status.phase === 'error' ? 'Retry' : 'Start a new shell'}
            </button>
          )}
        </div>
      )}
    </div>
  )
}
