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

// Web Push (`src/service/push.rs`): `{ title, body, url, tag }`.
self.addEventListener('push', (event) => {
  let data = {}
  try {
    data = event.data ? event.data.json() : {}
  } catch {
    data = { body: event.data ? event.data.text() : '' }
  }
  event.waitUntil(
    self.registration.showNotification(data.title || 'Peckboard', {
      body: data.body || '',
      tag: data.tag || undefined,
      renotify: Boolean(data.tag),
      icon: '/icon-192.png',
      badge: '/icon-192.png',
      data: { url: data.url || '/assistant' },
    }),
  )
})

// Focus an open Peckboard window on the notification's page, or open one.
// Only same-origin URLs: the payload must not send the user elsewhere.
self.addEventListener('notificationclick', (event) => {
  event.notification.close()
  let target = new URL('/assistant', self.location.origin)
  try {
    const url = new URL(
      (event.notification.data && event.notification.data.url) || '/assistant',
      self.location.origin,
    )
    if (url.origin === self.location.origin) target = url
  } catch {
    // keep /assistant
  }
  event.waitUntil(
    self.clients.matchAll({ type: 'window', includeUncontrolled: true }).then((windows) => {
      const open = windows.find((w) => new URL(w.url).origin === self.location.origin)
      if (!open) return self.clients.openWindow(target.href)
      return open
        .focus()
        .then((w) => (w && 'navigate' in w ? w.navigate(target.href) : w))
        .catch(() => self.clients.openWindow(target.href))
    }),
  )
})
