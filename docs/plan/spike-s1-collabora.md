# Spike S1: Collabora Online over WOPI (go)

30 October 2026. Spike S1 of [platform.md](platform.md) §6.

It ran Collabora Online Development Edition (CODE) 26.04.5.2, image `collabora/code` (447 MB
compressed for arm64). Collabora was started with
`aliasgroup1=http://host.docker.internal:18900` and TLS off for the test.

A WOPI host of about 170 lines stood in for Ferrum's Office app (`spikes/collabora_wopi_host.py`):
one file, the WOPI calls below, locks enforced, and each saved version kept.
`spikes/collabora_edit.mjs` drives headless Chrome.

## Verdict

**Go: Collabora is the default editor.** The WOPI surface Ferrum has to implement is small, and an
edit made in the browser came back as a valid DOCX.

## Checked

| What | Result |
|---|---|
| Discovery | `/hosting/discovery` lists 292 actions, including docx, xlsx, pptx and odt; the docx `edit` `urlsrc` is the editor page |
| Opening | the host page posts `access_token` to `urlsrc` + `WOPISrc` in a frame. Collabora made CheckFileInfo, GetFile and LOCK (with a lock id) within 0.5 s, and the document showed in editing mode |
| Editing | text typed in the browser |
| Saving | PutFile with the lock id, then UNLOCK, when the session ended (Collabora saves a modified document when its last user leaves, and on its autosave timer). The saved DOCX (10 parts) has the original and the typed text |
| Proof keys | none: this image publishes no `<proof-key>` and sends no `X-WOPI-Proof` |
| Terms | the editor opens with "Collabora Online Development Edition … free for you to use, to explore and to use with others"; CODE is the development edition |

## What changes in the plan (§4)

* **WOPI security without proof keys.** Ferrum checks proof signatures when the engine publishes a
  key: mount one into Collabora (`proof_key`), and Ferrum then requires it. Without a key, the
  defences are the per-user, per-file, short-lived access token, and WOPI endpoints that accept only
  the editor server's address (network policy). The same holds for ONLYOFFICE.
* **Locks are part of the contract.** Collabora locks on open, sends the lock id with PutFile and
  unlocks on close. Ferrum answers 409 with the current lock to a mismatch (as the spike host does)
  and expires locks after 30 minutes without RefreshLock.
* **Saving is the engine's job.** Versions come from PutFile: on autosave, explicit save, and the
  last user leaving. Drive keeps each PutFile as a version.
* **Production terms.** CODE is for development and evaluation. Production use means a Collabora
  Online subscription, or building from source, as the risk table says.
* Each tenant needs an `aliasgroup` (or a configured host list) naming the WOPI host's address.
