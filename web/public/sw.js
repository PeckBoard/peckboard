// Minimal service worker for PWA installability.
// Peckboard is a live-data app — we don't cache aggressively.

const CACHE_NAME = 'peckboard-v1'

self.addEventListener('install', () => {
  self.skipWaiting()
})

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((names) =>
        Promise.all(names.filter((name) => name !== CACHE_NAME).map((name) => caches.delete(name))),
      ),
  )
  self.clients.claim()
})

self.addEventListener('fetch', (event) => {
  // Network-first for everything — the app relies on live data.
  // Only fall back to cache for navigation requests (offline shell).
  if (event.request.mode === 'navigate') {
    event.respondWith(fetch(event.request).catch(() => caches.match('/index.html')))
  }
})
