/**
 * xterm's DOM renderer (the fallback when WebGL is unavailable) styles its
 * cells through `<style>` elements it injects and rewrites. The app's CSP
 * (`style-src 'self'`, src/security.rs) blocks inline `<style>`, which
 * leaves that renderer unstyled — wrong colours, wrong cell widths.
 *
 * Instead of loosening the app-wide policy, mirror each such element's text
 * into a constructable stylesheet (CSSOM — not subject to `style-src`) and
 * keep it in sync while xterm rewrites it. Scoped to one terminal's root.
 * Returns the cleanup.
 */
export function mirrorInlineStyles(root: HTMLElement): () => void {
  const sheets = new Map<HTMLStyleElement, CSSStyleSheet>()
  const adopt = (sheet: CSSStyleSheet) => {
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet]
  }
  const drop = (sheet: CSSStyleSheet) => {
    document.adoptedStyleSheets = document.adoptedStyleSheets.filter((s) => s !== sheet)
  }
  const scan = () => {
    for (const el of root.querySelectorAll('style')) {
      let sheet = sheets.get(el)
      if (!sheet) {
        sheet = new CSSStyleSheet()
        sheets.set(el, sheet)
        adopt(sheet)
      }
      try {
        sheet.replaceSync(el.textContent ?? '')
      } catch {
        /* malformed rule text: keep the last good copy */
      }
    }
    for (const [el, sheet] of sheets) {
      if (!el.isConnected) {
        drop(sheet)
        sheets.delete(el)
      }
    }
  }
  const observer = new MutationObserver(scan)
  observer.observe(root, { subtree: true, childList: true, characterData: true })
  scan()
  return () => {
    observer.disconnect()
    for (const sheet of sheets.values()) drop(sheet)
    sheets.clear()
  }
}
