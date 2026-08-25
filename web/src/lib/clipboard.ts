/** Copy `text` to the clipboard, reporting whether it worked.
 *
 *  `navigator.clipboard.writeText` is a secure-context API and can hang
 *  on a permission prompt — that's the "Copy does nothing" bug. PeckBoard
 *  is also routinely reached over plain HTTP on a LAN (`http://192.168.…`
 *  is NOT a secure context; `http://localhost` is). Prefer the synchronous
 *  `execCommand('copy')` path so a click handler still has a user gesture
 *  and cannot stall. Fall back to the Clipboard API only if that fails.
 */
export async function copyText(text: string): Promise<boolean> {
  if (copyTextFallback(text)) return true
  if (window.isSecureContext && navigator.clipboard?.writeText) {
    try {
      await Promise.race([
        navigator.clipboard.writeText(text),
        new Promise<never>((_, reject) => {
          setTimeout(() => reject(new Error('clipboard timeout')), 800)
        }),
      ])
      return true
    } catch {
      return false
    }
  }
  return false
}

/** Synchronous `execCommand('copy')` path. Must stay sync so a click-handler
 *  caller still has a user gesture. */
function copyTextFallback(text: string): boolean {
  try {
    const prev = document.activeElement as HTMLElement | null
    const ta = document.createElement('textarea')
    ta.value = text
    ta.setAttribute('readonly', '')
    ta.style.position = 'fixed'
    ta.style.top = '0'
    ta.style.left = '0'
    ta.style.width = '2em'
    ta.style.height = '2em'
    ta.style.padding = '0'
    ta.style.border = 'none'
    ta.style.outline = 'none'
    ta.style.boxShadow = 'none'
    ta.style.background = 'transparent'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.focus()
    ta.select()
    ta.setSelectionRange(0, text.length)
    const ok = document.execCommand('copy')
    document.body.removeChild(ta)
    prev?.focus?.()
    return ok
  } catch {
    return false
  }
}
