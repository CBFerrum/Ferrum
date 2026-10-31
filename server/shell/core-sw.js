// Ferrum's core worker at /sw.js, scope / (docs/plan/apps-ui.md §12.5): shows the notifications
// pushed to this device, puts the unread count on the installed app's icon, and opens what a
// notification is about. It has no fetch handler, so page loads never pass through it; offline
// apps keep workers of their own at /{route}/sw.js, whose narrower scopes control their pages.

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

/** A path on this host, which is all a message may open; anything else opens Home. */
const local = (url) => (typeof url === "string" && /^\/(?![/\\])/.test(url) ? url : "/");

self.addEventListener("push", (event) => {
  let n = {};
  try {
    n = event.data?.json() ?? {};
  } catch {}
  const id = typeof n.id === "string" ? n.id : null;
  const shown = self.registration.showNotification(typeof n.title === "string" && n.title.trim() ? n.title : "Ferrum", {
    body: typeof n.body === "string" ? n.body : "",
    // One notification per id: a message sent again replaces it rather than showing it twice.
    tag: id ?? undefined,
    icon: "/_shell/v1/icon-192.png",
    data: { url: local(n.url), id },
  });
  const nav = self.navigator;
  const badge = !Number.isInteger(n.unread) || !nav.setAppBadge ? null : n.unread > 0 ? nav.setAppBadge(n.unread) : nav.clearAppBadge();
  event.waitUntil(Promise.all([shown, Promise.resolve(badge).catch(() => {})]));
});

self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const { url, id } = event.notification.data ?? {};
  event.waitUntil(
    (async () => {
      if (id) await fetch(`/api/notifications/${encodeURIComponent(id)}/read`, { method: "POST", credentials: "same-origin" }).catch(() => {});
      const target = new URL(local(url), self.location.origin).href;
      const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      const open = windows.find((w) => w.url === target);
      return open ? open.focus() : self.clients.openWindow(target);
    })(),
  );
});

// The push service replaced this device's subscription: the new one goes to the server in the
// session the old one belonged to (the page's cookie goes along).
self.addEventListener("pushsubscriptionchange", (event) => {
  event.waitUntil(
    (async () => {
      let sub = event.newSubscription;
      if (!sub) {
        const key =
          event.oldSubscription?.options?.applicationServerKey ??
          (await fetch("/api/push", { credentials: "same-origin" })
            .then((r) => (r.ok ? r.json() : null))
            .then((p) => p?.key ?? null));
        if (!key) return;
        sub = await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      }
      await fetch("/api/push", { method: "PUT", credentials: "same-origin", headers: { "content-type": "application/json" }, body: JSON.stringify(sub.toJSON()) });
    })().catch(() => {}),
  );
});
