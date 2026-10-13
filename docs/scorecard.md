# Scorecard

The Scorecard judges delivery by outcomes: how long changes take to ship, what escapes, how recovery works, what review catches, and what successful changes cost. It is not an activity leaderboard. Decision: [ADR 0010](adr/0010-judge-delivery-by-outcomes.md).

The page is `/scorecard`, in the system navigation, and is admin-only like `/yield`. Select a 7, 30 or 90 day window, family and repository. Each tile shows its value, the immediately preceding equal-length window, a trend line, and a link to the underlying Flow lot, todo or pull request rows. Every tile includes a “How this is measured” note. Outcomes are monthly and live on a separate tab.

## Measurement contract

### Identity and time

- A **change** is one todo and the pull request(s) that carry it. Join by `Todo:` trailer / `todo_id`, retaining the todo as canonical identity. An unassociated PR can appear in PR context metrics but is not silently assigned to a todo.
- A **window** is the half-open UTC interval `[until - N days, until)`, where `until` is report generation time. Previous window is the immediately preceding interval of the same length. Trend points use consecutive, non-overlapping intervals of the selected length. Filters apply identically to both windows and drilldowns.
- A completion belongs to the window containing its completion event, unless a metric explicitly uses another event (for example, lead time uses deployed time). A change may contribute to different measures in different windows.
- Family and repository come from the joined Flow lot and event identity. Ambiguous joins are excluded from numerator and denominator and counted as unknown coverage, never duplicated.
- Percentages are `100 × numerator / denominator`. A zero denominator, insufficient source coverage, or missing required source yields `unknown` with a reason, not zero. Show eligible/sample counts with rates.
- Durations use UTC event receive times unless a source explicitly provides a more precise event timestamp. Negative, impossible or contradictory intervals are invalid and make the affected observation unknown. Percentiles use nearest-rank; report median (p50) and p85, never mean.
- Comparisons use the same definition, filters and source coverage in both windows. If coverage differs, display the coverage change. Do not connect trend points across unknown periods as if they were zero.

### Successful change and defect vocabulary

- A **successful change** reached production by deployment or, for a tag-released repository, by the first release tag containing the change; it was not reverted or reworked during the following 14 days and has no incident traced to it. The observation is pending until 14 days have elapsed and incident attribution for that period is available. Cost and human time per successful change divide only by this population, never by merged work.
- **False done**: a todo reported done whose change was later linked as reworked (todo front matter `reworks`) or reverted. Count once per todo.
- **Escaped defect**: a defect attributed to the change that caused it and discovered after passing the point being measured. Escaped to main means found after merge; escaped to prod means found after deploy. One defect may count at both levels, at most once at each level.
- **Review catch**: a defect finding made by a human or automated reviewer, after which code changed before merge. A finding without linked code change is not a catch. Distinct findings count separately only when they have distinct identities.
- **Rework**: a completed todo/change subsequently changed to correct or replace its delivered result, including a `reworks` link, replacement PR, or revert. A retry before completion is not rework.

## 1. Speed

Durations are reported in seconds with human-readable units. A sample needs both boundary events and a valid identity join.

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases |
|---|---|---|---|
| Lead time p50, p85 | For each shipped change: shipping timestamp minus `todo.filed`; report nearest-rank p50 and p85, never mean. | `flow_ledger` lot identity; `pipeline_events` `todo.filed`, successful `deploy.status`, and release events; release-tag membership is a new source. Bucket by ship event. | For deployed repos ship is successful production deploy. For tag-released repos use first tag containing change. Missing filed/ship/join makes sample unknown. Unshipped work is inventory, not a lead-time sample. |
| Station breakdown | Per shipped lot and station: `left_ms - entered_ms`, split into Flow waiting and active durations; show stacked station medians in Queue, Work, Shift, Review, Gate, Merge, Pin, Stage, Deploy order. Medians need not sum to line median. | `flow_ledger` station visits and event projection. Bucket by ship event. | Omit not-applicable stations. Missing entry/start/leave or waiting/active classification is unknown for that station, not zero. |
| Flow efficiency | Per shipped change: active time across applicable stations / total lead time; show median ratio and sample count. | `flow_ledger` active/waiting durations joined to filed and ship events; bucket by ship event. | Missing classified duration or invalid/zero total makes sample unknown. Do not assign unobserved intervals to active or waiting. |

For tag release, use the first repository tag whose tree contains the change's
commit (or a descendant commit containing it). Tag creation is the event time
only when the release source records it. A repository with production deploys
uses successful deployment as the endpoint.

## 2. Volume (context only)

Render headline volume in smaller type than outcome measures. Each headline
tile is adjacent to its named guardrail.

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases / paired guardrail |
|---|---|---|---|
| Completed work items/day | Distinct todos whose change reached main / elapsed window days. | `flow_ledger` Merge exit joined to todo, or `pipeline_events` `pr.merged`; bucket by merge time. | PR without todo excluded from todo count and shown in PR context. Pair with false-done rate. |
| Surviving net lines/day | Sum added minus removed lines for merged changes whose changed lines remain present 30 days after merge, divided by elapsed days. | New line-survival source: diff lineage at merge and +30 days. Bucket by merge date after day 30. | Pending observations shown separately. Track individual line identity, retaining moved-line identity where detectable. Pair with rework rate. |
| Releases/deploys per day per service | Distinct successful production releases/deployments per service / elapsed days. | `pipeline_events` `deploy.status` success; release tag source for tag-only repositories. | `release.staged` alone is not production. Missing service is unknown. Pair with change failure rate. |
| Changes per release (batch size) | Distinct changes first included in each release; median and p85 per service. | Release/tag membership source joined to lots and PRs. Bucket by release event. | Unknown membership makes batch unknown, not empty. Pair with change failure rate. |
| PRs merged/day (context) | Distinct PRs merged / elapsed days. | `pipeline_events` `pr.merged` / `queue.landed`; bucket by merge time. | Deduplicate queue landing and subsequent PR merge. Context only. |
| Raw net lines (context) | Sum added minus removed lines for merged diffs / elapsed days. | PR diff stats source; not currently durable. | Does not imply survival or value; display beside surviving lines and rework rate. |

## 3. Inventory

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases |
|---|---|---|---|
| WIP by station | At report time, count distinct lots with an open Flow visit, grouped by station and waiting/active/held state. | Current `flow_ledger` visits. | Missing transitions are unknown coverage; do not infer absence. A lot counts once at its current station. |
| Merged but unreleased | Count merged changes not yet in production deployment or release tag; age each from merge to now; show count and oldest age. | `flow_ledger` Merge; `pipeline_events` pin/release/deploy; tag membership source. | Not-applicable Pin is not a missing release. Unknown release membership shown separately. |
| Holds by age and action | Current held lots grouped by age since hold began, plus whether a recorded human action followed hold start. | `flow_ledger` hold direction and subsequent `todo.action`, `pr.review`, gate/release/deploy actions in `pipeline_events`. | “Acting” means a qualifying action event exists, not that hold resolved. Missing actor/action evidence is unknown. |
| Arrival vs completion rate | Todos filed/day and completed/day; gap = arrival minus completion, positive means measured backlog growth. | `pipeline_events` `todo.filed`; completion is change reaching main. | Divide by elapsed days. Retries/refiles do not create another arrival. Missing event coverage makes rate unknown. |

## 4. Quality

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases |
|---|---|---|---|
| Change failure rate | Distinct deployed changes later reverted, rolled back or tied to an incident / deployed changes with complete observation horizon. | New outcome ledger; `pipeline_events` `deploy.status`; new incident ledger and revert links. Bucket by deployment cohort after applicable observation period. | Count failed change once even if multiple signals apply. Pending horizon or missing attribution is excluded and shown. |
| Escaped defects to main | Defects found after merge and attributed to cause change / merged changes with complete defect-source coverage. Show defect count and affected-change count too. | New outcome ledger and review finding outcomes joined to merge. | Count defects by discovery time and rate by merge cohort, labeled separately. Missing attribution is unknown, not zero. |
| Escaped defects to prod | Defects found after deploy and attributed to cause change / deployed changes with complete defect-source coverage. Show defect and affected-change counts. | New outcome ledger and incident ledger joined to successful `deploy.status`. | Same defect may count at both main and prod levels; distinct defect id once at each level. |
| False-done rate | Todos reported done whose change was later reworked or reverted / todos reported done with complete 14-day observation. | `todo.attempt_finished` outcome `done`, front matter `reworks`, revert/rework links in outcome ledger. Bucket by done time. | Later outcome updates numerator. Pending 14-day observations shown separately. |
| Review catch rate | Distinct review findings followed by code change before merge / (such catches + defects that escaped past review). | New review finding outcome source joined to `review.started`/`review.finished`, PR head changes and outcome ledger. | Finding without code change is not catch. Missing finding/escape coverage is unknown; verdict alone proves no catch. |
| Rework rate | Completed changes subsequently reworked / completed changes with 14-day observation. | Outcome ledger from `reworks`, replacement PR links, reverts; todo completion events. Bucket by completion cohort. | Pending horizon separate. Retry before done is excluded. |
| Main red time per repo | Seconds protected main is known red / seconds with known main health, by repo. | New main-health transition source. | Unknown gaps excluded from denominator and shown as coverage. No records means unknown, not 0%. |
| Gate flake rate | Gate attempts that fail then pass on the same commit / attempts with terminal pass or fail on that commit. | `pipeline_events` `gate.finished` / `gate.log`, joined by repo, PR, SHA and run identity. | Retry on different SHA is not a flake. Missing SHA/run identity cannot be paired and is unknown. |

## 5. Recovery

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases |
|---|---|---|---|
| Time to detect | Per incident: incident opened timestamp minus cause deployment timestamp; report p50 and p85. | New incident ledger linked to deployment/outcome ledger. Bucket by incident-opened time. | No uniquely attributed cause deployment is unknown. Negative duration invalid. |
| Time to restore | Per incident: first successful fix deployment minus incident-opened timestamp; report p50 and p85 separately. | New incident ledger and successful `pipeline_events` `deploy.status`, linked to fix change. Bucket by incident-opened time. | Open incidents have no duration sample and appear as open age. Never combine with time to detect or label either as MTTR. |

## 6. Economics

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases |
|---|---|---|---|
| Cost per successful change | Sum attributable USD cost / successful-change count; show both. | Existing `cost_ledger` joined by todo, PR, repo and family; successful population from new outcome ledger. | Sum only costs attributable to included changes. Show unattributed spend separately. Pending success observations excluded; zero denominator is unknown. |
| Human minutes per successful change | Sum human active minutes on included successful changes / successful-change count. | New human-time source joined to change and successful outcome. | Exclude waiting; source distinguishes active work. Missing effort coverage is unknown and shown. |
| Review cost | Review minutes and USD per reviewed change; show totals and median per change. | `pipeline_events` `review.started`/`review.finished`, `pr.reviewed` cost/seconds in `cost_ledger`; reviewer kind. | Pair intervals by review/run identity and SHA; overlapping sessions are not double-counted as elapsed time. Missing start/end/cost leaves that component unknown. |

## 7. Predictability and capacity

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases |
|---|---|---|---|
| ETA error | Per lot, absolute actual completion minus ETA captured at a defined snapshot; report median absolute error and signed bias separately. | New immutable Flow ETA snapshot source; actual endpoint from Flow completion/deployment event. | Recomputed historical ETA is not evidence. Missing snapshot is unknown. |
| Lead-time spread | p85 minus p50 lead time from Speed's same eligible cohort. | Flow and shipping events as Speed. | Unknown if either percentile unavailable; never use mean. |
| Gate queue wait | Gate entry to gate start; report p50 and p85. | `flow_ledger`; producer `gate.queued` event is new where entry cannot be proved. | Without proven queue-entered event, unknown; do not use review time as proxy. |
| Worker slot utilisation | Worker busy seconds / reporting worker slot seconds, by pool and window. | New slot-state history derived from worker heartbeats / `worker.stage`. | Missing heartbeats are unknown time, not idle. Show coverage. |
| Runner utilisation | Busy runner seconds / reporting runner seconds by runner/service. | Heartbeat history; durable history is new if not retained. | Missing heartbeat is not idle. Unconfigured capacity is unknown. |

## 8. Work mix

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases |
|---|---|---|---|
| Completed work mix | For feature, fix, rework and ops: distinct completed todos in class / all classified completed todos. Show shares and counts. | Completion events and new/explicit work classification field; Flow merge join. Ops includes pins, red-main repair and replacement PRs. | One primary class per change. Unclassified count shown as unknown, not distributed. |

## 9. Outcomes (monthly tab)

| Tile | Numerator / denominator or statistic | Event source and window | Edge cases |
|---|---|---|---|
| Hypotheses checked and met | Shipped ideas whose stated hypothesis was checked and met / shipped ideas with evaluation due in month. Show checked share = checked / eligible and met share = met / checked separately. | New hypothesis field linking idea, measurable result, owner and check date; shipped event from release/deploy source. Bucket by scheduled check month. | Missing hypothesis or unrecorded check is not met. Future check dates excluded until due. |

## Sources and availability

“Existing” means recorded in jeryu-deploy at this decision, not that every
event is present. Missing events remain unknown.

| Source | Availability and use |
|---|---|
| `flow_ledger` | Existing durable lot/station intervals and outcomes; WIP, station durations and lot links. Does not provide every station entry/start or immutable ETA snapshots. |
| `cost_ledger` | Existing durable event costs. Does not prove human effort or successful outcome membership. |
| `pipeline_events` | Existing `todo.*`, `pr.*`, `gate.*`, `release.*`, `deploy.*`, `pin.*`, `review.*`, `worker.stage` kinds per `docs/pipeline-events.md`; proves transitions only. Event retention is 30 days, so history needs durable projections. |
| Outcome ledger | New: defect identities and attribution, rework/revert, escaped-to-main/prod, failure and successful-change observation state. |
| Incident ledger | New: incident open/detect/restore timestamps and causal deployment/change links. |
| Review finding outcomes | New: finding identity, defect classification, reviewer kind, code-change-before-merge outcome and escaped linkage. |
| Main health | New: per-repo protected-main red/green transitions and coverage. |
| Line survival | New: diff-line identity/lineage snapshots at merge and 30 days later. |
| Human time | New: attributable active human minutes by change and activity type. |
| Hypothesis field | New: stated hypothesis, evaluation measure, due/check date and met result linked to shipped idea. |
| Release membership | New where release events or tags do not prove every included change; must identify first containing tag for tag-released repos. |
| ETA snapshots, slot history, classification | New where Flow/event sources do not retain immutable ETA snapshots, worker capacity history or explicit work class. |

These sources are separate todos. A tile depending on an absent source displays
`unknown — source not recorded`, names it in its measurement note, and links
to whatever rows exist. Public product and documentation use invented
examples only (for example families `acme` and `globex`, host `node-a`); never
expose real hostnames, filesystem paths or personal names.
