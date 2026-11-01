# Security

## Reporting a vulnerability

Please do not open a public issue. Use **Report a vulnerability** under this repository's Security tab,
which reaches the maintainers privately. Say what you found, how to reproduce it, and what it lets
someone do. If that form is not available, open an issue asking for a private channel, with no details.

Testing against a server you do not run needs its owner's permission.

## What happens next

The report is acknowledged, the problem is reproduced, and a fix is prepared in private. Reporters are
credited when the fix is released, unless they prefer not to be. Which releases get fixes is in the
[support policy](docs/support-policy.md).

Advisories in dependencies that do not reach Ferrum, or that have no fix yet, are recorded with an owner,
a reason and a review date in [docs/registry/advisories.yaml](docs/registry/advisories.yaml), and CI
fails when a review date passes.

## What is in place

[docs/security.md](docs/security.md) describes how Ferrum protects data, and
[docs/registry/threat-model.md](docs/registry/threat-model.md) what it defends against. Ferrum has no
security certification, and has not been through an independent penetration test yet.
