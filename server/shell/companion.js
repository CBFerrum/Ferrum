// The page where someone confirms it is them before a program beside Ferrum signs them in (server/src/controllers/oidc.rs,
// `confirm_page`). It asks the server to finish the sign-in; until the person has confirmed it is them as recently as the
// program asked, the server answers with its step-up refusal, which the shell's runtime (loaded before this) answers by
// asking them in one of their ways and sending the request again. Then the browser goes on to the program.

const data = (() => {
  try {
    return JSON.parse(document.getElementById("ferrum-companion")?.textContent ?? "{}");
  } catch {
    return {};
  }
})();
const error = document.getElementById("ferrum-companion-error");
const again = document.getElementById("ferrum-companion-go");

async function finish() {
  if (again) again.disabled = true;
  try {
    const r = await fetch("/api/oauth/authorize/confirm", {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json", accept: "application/json" },
      body: JSON.stringify({ waiting: data.waiting }),
    });
    const body = await r.json().catch(() => ({}));
    if (r.ok && typeof body.redirect_to === "string") {
      location.replace(body.redirect_to);
      return;
    }
    if (error) error.textContent = typeof body.message === "string" ? body.message : data.failed;
  } catch {
    if (error) error.textContent = data.failed;
  }
  if (again) again.disabled = false;
}

again?.addEventListener("click", finish);
finish();
