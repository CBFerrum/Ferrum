# Licensing: what is settled, and what needs a lawyer

The specification (§12.6) asks that every artifact's terms be stated exactly, and warns against
advertising unrestricted proprietary-plugin compatibility until the architecture and the terms support
it. This records the facts. It is not legal advice, and the open questions below are not engineering
decisions.

| Artifact | License | Where it says so |
|---|---|---|
| Ferrum server and crates | AGPL-3.0-or-later | `Cargo.toml`, `LICENSE` |
| Native apps (`ferrum-app-*`) | AGPL-3.0-only | each repository's `Cargo.toml` |
| `ferrum-sdk` (CLI, guest SDK, templates) | Apache-2.0 | its `Cargo.toml`, `LICENSE` |
| Dependencies | [docs/licenses.md](../licenses.md), checked in CI | generated |
| Frontend's bundled packages | each release's SBOM | CI's `promote` |
| Third-party editors and renderers | their own, run as separate services | `deploy/` |

Native apps are compiled into the server, so the combined binary's terms are at least AGPL-3.0, and the
`-only` of the apps fixes it at version 3. Apps that are not native fall into two groups:

- **Sandboxed plugins and bundles** run in Wasmtime and reach Ferrum through a narrow host interface.
- **Isolated apps** run on their own origin, or as separate services.

## Open questions for counsel

Nothing below should be stated publicly until someone with the authority to answer has answered:

1. Whether a proprietary plugin talking to the host through the sandbox's interface is a separate work
   under the AGPL, or whether some designs of it are not.
2. Whether the Apache-2.0 guest SDK, once compiled into a plugin that runs inside an AGPL host, leaves
   that plugin free of the AGPL.
3. What the AGPL's network clause requires of Ferrum Cloud and of partners who host it, and what a
   modified installation must offer its users.
4. Whether `AGPL-3.0-only` on the apps and `-or-later` on the core is what is wanted.
5. What contributors' terms are needed (a CLA or DCO) before any dual licensing could happen.
