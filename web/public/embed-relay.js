// Runs inside the sandboxed `srcdoc` wrapper of a link-preview embed
// (opaque origin; see srcdocFor in web/src/components/LinkPreview.tsx).
// The provider's widget frame inside posts its height to this wrapper,
// never to Peckboard itself; relay just that number to the parent page.
;(function () {
  function frame() {
    return document.querySelector('iframe')
  }
  document.addEventListener('DOMContentLoaded', function () {
    document.documentElement.style.margin = '0'
    document.body.style.margin = '0'
    var f = frame()
    if (f) {
      f.style.border = '0'
      f.style.display = 'block'
    }
  })
  window.addEventListener('message', function (e) {
    var f = frame()
    if (!f || e.source !== f.contentWindow) return
    var d = e.data
    if (typeof d === 'string') {
      try {
        d = JSON.parse(d)
      } catch (_) {
        return
      }
    }
    if (!d || typeof d !== 'object') return
    var h = null
    var tw = d['twttr.embed']
    if (tw && tw.method === 'twttr.private.resize' && tw.params && tw.params[0]) {
      h = Number(tw.params[0].height)
    } else if (typeof d.height === 'number') {
      h = d.height
    }
    if (!h || !isFinite(h) || h <= 0) return
    h = Math.ceil(h)
    f.height = String(h)
    window.parent.postMessage({ type: 'peckboard-embed-resize', height: h }, '*')
  })
})()
