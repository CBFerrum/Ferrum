// Served at /{route}/sw.js where no app the person may use lives any more (uninstalled, access
// removed, signed out): the worker clears the caches of its own route, removes itself and
// reloads its pages, so nothing stale keeps answering there (docs/plan/apps-ui.md §12.5).
// Caches of other apps on the host are left alone: apps name theirs "ferrum:{route}:…".

self.addEventListener("install", () => self.skipWaiting());

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const route = new URL(self.registration.scope).pathname.split("/").filter(Boolean)[0] ?? "";
      for (const key of await caches.keys()) {
        if (route && key.startsWith(`ferrum:${route}:`)) await caches.delete(key);
      }
      await self.registration.unregister();
      for (const client of await self.clients.matchAll({ type: "window" })) client.navigate(client.url);
    })(),
  );
});
