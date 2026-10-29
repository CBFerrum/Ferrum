# Spike S2: Stalwart as the mail server (go)

30 October 2026. Spike S2 of [platform.md](platform.md) §6, run against Stalwart **1.0.1**, the
open-source edition (AGPL-3.0, image `stalwartlabs/stalwart:latest` of 2026-10-28, 116 MB).

`spikes/stalwart_spike.py` repeats it from scratch:

    DOCKER_CONFIG=<dir with an empty config.json> python3 docs/plan/spikes/stalwart_spike.py

It starts a fresh container, completes the bootstrap, provisions, then mails as the webmail
would: 22 checks, all passing.

## Verdict

Go. Three facts change the plan:

1. **Administration is JMAP, not REST.** Stalwart 1.0 removed the REST management API:
   `/api/principal` is gone. Everything is a JMAP object under `urn:stalwart:jmap`, with the full
   schema at `/api/schema`. Examples: `x:Account` (users and groups), `x:Domain`, `x:MailingList`,
   `x:DkimSignature`, `x:SpamRule`, `x:SieveSystemScript`, `x:AppPassword`, `x:OAuthClient`,
   `x:Role`. Ferrum's mail add-on therefore speaks one protocol, JMAP, for both administration and
   mail.
2. **Multi-tenancy and SCIM are Enterprise features.**
   * `x:Tenant` is marked `enterprise: true`. 1.0.1 accepts the object without a licence; whether
     it isolates anything without one was not checked.
   * SCIM answers `403 SCIM provisioning is only available in the Enterprise edition`.
   * Also Enterprise: AI spam models (`x:SpamAi`, `x:AiModel`), archiving (`x:ArchivedItem`),
     masked email, alerts, metrics and tracing.
3. **Group accounts are the shared mailboxes.** A group account (`sales@`) shows up as an extra
   JMAP account for each member. There is no separate shared-mailbox concept to build.

## Checked

| Need | How (JMAP unless said) | Result |
|---|---|---|
| First start | the temporary admin printed once, on stderr; `x:Bootstrap/set` (domain, hostname, no ACME) returns the permanent administrator; restart | ok |
| Domain | the bootstrap's domain; `x:Domain/set` `allowScimProvisioning` | ok |
| Users | `x:Account/set` `@type: User`, password credential, alias, `memberGroupIds` | ok |
| Groups | `x:Account/set` `@type: Group` | ok |
| Mailing list | `x:MailingList/set` with `recipients` | ok |
| DNS to publish | `x:Domain` `dnsZoneFile`: MX, SPF, DKIM (Ed25519 and RSA), DMARC, MTA-STS, TLS-RPT, SRV for IMAP/JMAP/submission/CalDAV/CardDAV, autoconfig | ok |
| Mailboxes | `Mailbox/get`: inbox, drafts, sent, junk, trash | ok |
| Send | `Email/set` + `EmailSubmission/set`, to a user and to the group | delivered to both |
| Spam | the GTUBE test message from an outside sender over SMTP | landed in Junk |
| Training | "not spam": move out of Junk with `$notjunk` | a training sample (`x:SpamTrainingSample`) recorded |
| Vacation reply | `VacationResponse/set` | ok |
| Sharing | `Mailbox/set` `shareWith {<account id>: {mayReadItems, mayAddItems}}` | the other user sees it as an account |
| SCIM | `/scim/v2/ServiceProviderConfig` | 403, Enterprise |

Present but not exercised:
* Sieve (`urn:ietf:params:jmap:sieve`).
* Calendars, contacts and file storage over JMAP, with CalDAV and CardDAV records in the zone.
* Publishing DNS records through providers (`x:DnsServer`: Cloudflare and others).
* An OpenID provider of its own.
* External directories: LDAP, SQL, or OpenID Connect (`x:OidcDirectory`: issuer, username and
  group claims, audiences, token introspection).

## Gotchas

* JMAP object lists are maps keyed by index (`{"0": {...}}`); sets are maps of `true`.
* The temporary administrator's password appears once, on stderr. Deployments either automate the
  bootstrap or set `STALWART_RECOVERY_ADMIN`.
* On this machine Docker Desktop's credential helper hung on every pull. Pulling with an empty
  `DOCKER_CONFIG` avoided it.

## What changes in the plan (§5)

* **Provisioning:** the JMAP administration API, not REST or SCIM. Ferrum syncs idempotently from
  its events:
  * users → `x:Account` `User`;
  * teams and departments → `x:Account` `Group`, which are also the shared mailboxes;
  * lists → `x:MailingList`;
  * mail domains → `x:Domain`.
* **Hosting:** in the open-source edition, one Stalwart per workspace that hosts its mail. The
  image is small, and the workspace's domains bound it. An Enterprise licence is only for many
  workspaces on one instance.
* **The domain page** shows `dnsZoneFile` and checks each record live.
* **Still to prove in Phase 3:**
  * the OpenID Connect directory against Ferrum, once Ferrum publishes discovery and its signing
    keys;
  * outbound delivery through a relay;
  * a round trip through the Sieve rule editor.
