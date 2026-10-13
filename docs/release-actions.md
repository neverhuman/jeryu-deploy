# Release actions: one click for a release step

A deployer can ask the forge for a release step from the attention inbox (the "Waiting on you"
rows on Needs you and Releases). The forge records the ask; the release host's
`jeryu-release-actions.timer` (`scripts/release/release-action.sh poll`) claims it and runs it.

| Action | Argument | What the release host runs |
| --- | --- | --- |
| `deploy` | `release`, a staged id `prod-<UTC stamp>-<sha>-unsigned` | `deploy-release.sh <release>` from the commit named by the staged release, after auto-stage is idle |
| `auto_pin` | none | `systemctl --user start jeryu-auto-pin.service` |
| `auto_pin_retry` | `head`, the 40-hex jeryu-web head auto-pin gave up on | forget its failure count, then start auto-pin |

## Routes

- `POST /api/v1/release-actions` `{action, repo, release?|head?}`: records an ask. 201 with the
  ask, or 200 with the open ask for the same step (a second click is the first). A `deploy` must
  name a release the forge has a `release.staged` event for, for that repository (409 `not_staged`
  otherwise); malformed fields are 422. An `auto_pin_retry` requires a `pin.bump_failed` event with `needs_human` for this exact repository and head (409 `not_retryable` otherwise). The queue holds at most 256 asks; a full open queue returns 429 `release_queue_full`.
- `GET /api/v1/release-actions?state=pending|all`: the poller reads `pending`.
- `POST /api/v1/release-actions/:id/claim`: takes a pending ask once (409 `already_claimed` after).
- `POST /api/v1/release-actions/poller` `{repo}`: a deployer advertises its repository poller for 90 seconds. Runner heartbeat labels cannot advertise it.
- `POST /api/v1/release-actions/:id/cancel`: atomically cancels a pending ask; a claimed step cannot be stopped by cancelling an ask.
- `POST /api/v1/release-actions/:id/result` `{ok, message?, log_tail?}`: how a claimed ask ended.

Every route needs a **deployer**: an admin named in `JERYU_DEPLOYERS`, the same bar as recording a
deployment. Admin alone is not enough, because the gate and reviewer identities are admins whose
tokens the code they check can read.

## Safety

- The forge never sends a command: an ask is one of the three actions above and its argument,
  validated on the forge and again on the release host, which maps it to fixed steps.
- The poller filters asks for its own repository (`JERYU_DEPLOY_REPO`) before selecting the oldest, one per tick. Other repositories cannot starve it.
- Pending asks expire after 15 minutes. Claimed asks expire after 90 minutes, longer than the poller's 75-minute systemd timeout. Expiry records an **unknown host outcome**, does not stop a host process, and never automatically retries it. Check the release host and durable deployment records before requesting another step.
- The launcher resolves the release ID's commit prefix uniquely in protected main's fetched history, verifies ancestry, and executes scripts from that immutable commit. Missing, ambiguous or off-main commits are refused.
- Only unset `JERYU_*` settings are loaded from `release.env`; the file is never executed.
- Result messages and log tails redact bearer credentials, PATs, common secret assignments, URL credentials and URL query strings before upload. Raw 0600 logs stay on the release host; operators must keep other sensitive host output out of logs.
- Asks live in memory. A deploy restarts the forge, so the result of that deploy may be lost (the
  poller treats a 404 as fine); the deployment record `deploy-release.sh` writes is the durable
  answer, and a restarted forge replays nothing.

## The inbox

`release_staged` and `pin_behind` rows carry `action.api` (the button, with a `confirm` text that
says the release host runs it) only while a deployer-authenticated poller check-in for that repository is fresh and no
ask for the same step is open. While one is open, the row says who asked and when instead of
offering a second button. Without a poller the rows keep their copyable command only.

The pinned web commit `83534746af1bd2aa240456d65628fb736bdd2199` passes `action.confirm` to `ActionPreviewDialog` in `apps/web/src/pages/needsYou/AttentionRow.tsx` before sending the request.
