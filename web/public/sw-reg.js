// Registers the shell service worker. Failures are silent — the surface
// works fully online without it.
if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js').catch(() => {});
}
