# Social: references for connections, part three

**Read:** Postiz (AGPL-3.0, `referance/social/postiz` at `23f29c4`) and Mixpost Lite (MIT, `referance/social/mixpost`
at `df57648`), through the audit in `ERP/gaps-social/postiz.md` and `mixpost.md` (13 November 2026). Nothing was
copied: they are TypeScript and PHP, and each network is code there. What was taken is the behaviour the networks
demand, which the platform now lets a description say (docs/builder/decisions.md BD-53 to BD-62). Paths below are in
each repository.

| Part | What it answers in the references |
|---|---|
| P1 steps (BD-53) | Postiz's `postPending` / `checkPostStatus` / `finalizePost` with its arm, confirm, publish handshake (`libraries/nestjs-libraries/src/integrations/social.abstract.ts`, `apps/orchestrator/src/workflows/post-workflows`), and each provider's upload-then-post (`linkedin.provider.ts` `initializeUpload`, `reddit.provider.ts` `uploadFileToReddit`, `bluesky.provider.ts` `comment` needing the post's `cid`); Mixpost's per-network jobs (`src/SocialProviders/*/Jobs`). Here a step's lost answer is settled by an idempotency header or a read-back (`confirm`), never sent blindly. |
| P2 wait (BD-54) | Instagram's and Threads' container `status_code` polling, TikTok's `publish/status/fetch`, YouTube's session probe, Mastodon's `206` while media processes, LinkedIn's `AVAILABLE` (the providers' `checkPostStatus`); Postiz's 20 s loops with an 8-minute cap. |
| P3 loops (BD-55) | X's chunked `initialize` / `append` / `finalize` (`x.provider.ts`), YouTube's resumable 8 MB chunks with `Content-Range` and its `bytes */size` probe (`youtube.provider.ts`), TikTok's chunks with the remainder in the last (`tiktok.provider.ts`), LinkedIn's `uploadInstructions` with ETags (`linkedin.provider.ts`), Postiz's ranged reads (`mediaChunk`); threads as reply chains. |
| P4 functions and clock (BD-56) | Bluesky's record key (`TID`) and facets with byte offsets (`bluesky.provider.ts`, `detectFacets`), X's weighted length (not done: its weights are X's), `createdAt` repeated on a retry. |
| P5 Retry-After and limits (BD-57) | Postiz's `maxConcurrentJob` per provider and its 429 retries (`uploadWithRateLimitRetry`); Mixpost's rate-limit handling in `src/SocialProviders/*/Concerns`. |
| P6 as system (BD-58) | Social 0.7.0's own debt: `external_id` made writable for a person's pull. |
| P7 XML and forms (BD-59) | Postiz's autopost feeds (`apps/orchestrator/src/workflows/autopost.workflow.ts`), Reddit's S3 `<Location>` answer, form-encoded token answers. |
| P8 per item and the account (BD-60) | Postiz's `postAnalytics` per post and `analytics` by account id; Mixpost's per-account metrics jobs. |
| P9 sign-in extras (BD-61) | Facebook's `fb_exchange_token` (`facebook.provider.ts`), Instagram's and Threads' `refresh_access_token` (`instagram.standalone.provider.ts`, `threads.provider.ts`), VK's and TikTok's extra token fields, Mastodon's per-instance `POST /api/v1/apps` (`mastodon.custom.provider.ts`; Mixpost's `src/SocialProviders/Mastodon`), Mastodon's never-expiring tokens. |

Not taken, on purpose: OAuth 1.0a (X is described on OAuth 2), Nostr and Farcaster key signing, the browser
extension's cookie sign-in, Postiz's provider classes (a network is a description here, BD-29).
