// Running inside the PeckBoard mobile app (apps/mobile): its WebView appends
// ` PeckBoardApp/<version>` to the user agent, and the app's loopback gate
// stores the shell's origin in the `__pbm_shell` cookie, so the box UI can
// send the WebView back to the app's box list without any IPC.

const APP_UA = /\bPeckBoardApp\/\S+/
const SHELL_COOKIE = '__pbm_shell'

/** Tauri shell origins (iOS custom scheme, Android, dev server). Anything
 *  else in the cookie is ignored — never an open redirect. */
export function isShellOrigin(origin: string): boolean {
  return (
    origin === 'tauri://localhost' ||
    origin === 'http://tauri.localhost' ||
    origin === 'https://tauri.localhost' ||
    /^http:\/\/localhost:\d{1,5}$/.test(origin)
  )
}

export function isMobileApp(): boolean {
  return typeof navigator !== 'undefined' && APP_UA.test(navigator.userAgent)
}

/** The app shell's URL to return to, or null outside the app. */
export function mobileShellUrl(): string | null {
  if (!isMobileApp() || typeof document === 'undefined') return null
  for (const part of document.cookie.split(';')) {
    const [name, ...rest] = part.trim().split('=')
    if (name !== SHELL_COOKIE) continue
    let origin: string
    try {
      origin = decodeURIComponent(rest.join('='))
    } catch {
      return null
    }
    return isShellOrigin(origin) ? `${origin}/` : null
  }
  return null
}
