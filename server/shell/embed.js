// What another site puts beside a framed public page of a workspace (docs/builder/decisions.md BD-80): it sizes each
// `<iframe data-ferrum-embed>` to the height its page posts, and tells the site when the page is done
// (`ferrum:done`, bubbling from the frame). It takes a message only from the frame's own window and the frame's own
// origin, reads nothing of the site's page, writes no HTML and runs nothing it is sent. Served on every host of the
// platform at /_shell/v1/embed.js, the one platform file another origin may load.
(() => {
  const frames = new Map(); // iframe → its page's origin

  function watch(f) {
    if (frames.has(f)) return;
    try {
      frames.set(f, new URL(f.src, document.baseURI).origin);
    } catch {
      // Not an address: nothing can come from it.
    }
  }

  function scan(root) {
    if (root.matches?.("iframe[data-ferrum-embed]")) watch(root);
    root.querySelectorAll?.("iframe[data-ferrum-embed]").forEach(watch);
  }

  window.addEventListener("message", (e) => {
    let frame = null;
    for (const [f, origin] of frames) {
      if (f.contentWindow === e.source && e.origin === origin) frame = f;
    }
    if (!frame) return;
    const d = e.data;
    if (d?.ferrum === "height" && Number.isFinite(d.height)) {
      frame.style.height = Math.min(20000, Math.max(80, Math.ceil(d.height))) + "px";
    } else if (d?.ferrum === "done") {
      frame.dispatchEvent(new CustomEvent("ferrum:done", { bubbles: true }));
    }
  });

  scan(document);
  new MutationObserver((changes) => {
    for (const c of changes) c.addedNodes.forEach((n) => n.nodeType === 1 && scan(n));
  }).observe(document.documentElement, { childList: true, subtree: true });
})();
