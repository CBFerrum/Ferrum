// The accounts host's page where a person confirms it is them with a passkey of their Ferrum account, for a workspace
// whose address may not use it (docs/operations.md, "Asking again before sensitive work"). The dialog on the
// workspace's address opened this window (`/step-up?state=…`). The passkey is asked for here, fresh: whoever is signed
// in here proves nothing. Once the server accepts it, the window goes back to the workspace's address with a one-use
// proof, which the dialog there takes. Nothing here signs anyone in.

const boot = JSON.parse(document.getElementById("ferrum-step-up").textContent);

// The page's words in the browser's language (server/shell/locales), English where the catalog has none.
const WORDS = boot?.locale?.shell ?? {};
function tr(english) {
  return WORDS[english] ?? english;
}

// The request travels in the address only this far: out of the window's history (and never in a referrer).
const state = new URLSearchParams(location.search).get("state") ?? "";
history.replaceState(null, "", location.pathname);

const go = document.getElementById("ferrum-step-up-go");
const error = document.getElementById("ferrum-step-up-error");
const say = (text) => (error.textContent = text);
const GONE = tr("This request to confirm it is you has expired or was used already. Close this window and try again.");
const BUSY = tr("Too many tries. Wait a moment and try again.");
const FAILED = tr("Could not confirm it is you. Try again.");

// A passkey's binary fields travel as base64url (as login.js sends them).
const b64 = {
  decode: (s) => Uint8Array.from(atob(s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4)), (c) => c.charCodeAt(0)),
  encode: (buf) => btoa(String.fromCharCode(...new Uint8Array(buf))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, ""),
};

async function post(path, body) {
  const r = await fetch(path, {
    method: "POST",
    credentials: "same-origin",
    headers: { "content-type": "application/json", accept: "application/json" },
    body: JSON.stringify(body),
  });
  return { status: r.status, data: await r.json().catch(() => null) };
}

/** Asks the device for the passkey and sends its answer; accepted, the window goes back to the workspace. Once the
 *  request is used up or gone, the button stays off: the dialog that opened this window starts a new one. */
async function prove() {
  say("");
  go.disabled = true;
  let again = true;
  try {
    const offer = await post("/api/accounts/step-up/options", { state });
    if (offer.status !== 200 || !offer.data?.publicKey) {
      again = offer.status === 429 || offer.status >= 500;
      return say(offer.status === 429 ? BUSY : again ? FAILED : GONE);
    }
    const o = offer.data.publicKey;
    let cred = null;
    try {
      cred = await navigator.credentials.get({
        publicKey: { ...o, challenge: b64.decode(o.challenge), allowCredentials: (o.allowCredentials ?? []).map((c) => ({ ...c, id: b64.decode(c.id) })) },
      });
    } catch (e) {
      if (e?.name !== "NotAllowedError" && e?.name !== "AbortError") return say(FAILED);
    }
    if (!cred) return say(tr("No passkey was used. If this device has no passkey of your Ferrum account, use one that has, or close this window."));
    const a = cred.response;
    const response = {
      clientDataJSON: b64.encode(a.clientDataJSON),
      authenticatorData: b64.encode(a.authenticatorData),
      signature: b64.encode(a.signature),
      userHandle: a.userHandle ? b64.encode(a.userHandle) : null,
    };
    const answer = await post("/api/accounts/step-up", { state, request: offer.data.request, credential: { id: cred.id, rawId: b64.encode(cred.rawId), type: cred.type, response } });
    const next = answer.data?.redirect;
    if (answer.status === 200 && typeof next === "string" && /^https?:\/\//.test(next)) {
      again = false;
      location.replace(next);
      return;
    }
    again = answer.status === 429 || answer.status >= 500;
    say(answer.status === 429 ? BUSY : again ? FAILED : tr("That passkey was not accepted, or this request has expired. Close this window and try again."));
  } catch {
    say(FAILED);
  } finally {
    go.disabled = !again;
  }
}

go.addEventListener("click", () => void prove());
document.getElementById("ferrum-step-up-close").addEventListener("click", () => window.close());
if (!state) {
  go.disabled = true;
  say(GONE);
} else if (!window.PublicKeyCredential || !window.isSecureContext) {
  go.disabled = true;
  say(tr("This browser cannot use a passkey. Use another browser, or close this window."));
}
(go.disabled ? document.getElementById("ferrum-step-up-close") : go).focus();
