# Pull request workflow view

`GET /api/v1/repos/{id}/pulls/{number}` carries one extra block, `workflow`,
beside the existing detail fields. It needs the same access as the detail (no
admin control-plane access) and adds no mergeability rule: the outcome is read
off the merge passport the detail already computes.

| Field | Meaning |
| --- | --- |
| `schema` | `pr_workflow/v1` |
| `outcome` | `draft`, `checks_pending`, `checks_failed`, `review_needed`, `blocked`, `ready`, `closed`, `merged`, or `head_changed` |
| `head_sha` / `current_head_sha` | the head observed, and the head read again afterwards |
| `lifecycle`, `draft`, `base_ref` | open/closed/merged and draft state |
| `can_merge` | true only for `ready` |
| `required_checks[]` | `name`, `state` (`passing`, `failing`, `pending`, `missing`), `head_sha`, `evidence_url` |
| `other_checks[]` | checks on the head that are not required; never counted |
| `review` | approval counts and `reviews[]` with each review's own `head_sha` and `counts_for_head` |
| `passport` | `status`, `head_sha`, `hash`, `blockers[]` (`code`, `message`) |
| `next_action` | `code`, `label`, and `method`/`href` where a route performs it |

Rules:

- Checks are read on the observed head, and reviews count only when recorded
  on it. A result on an older head never moves the outcome toward `ready`.
- If the pull request has a different head when the block is re-read, the
  outcome is `head_changed` and the next action is to fetch again.
- Outcome priority: lifecycle (`merged`, `closed`), `draft`, `ready`, then a
  non-check/non-review blocker (`blocked`), `checks_failed`, `checks_pending`
  (including a context that has not run), `review_needed`.
- Only hosted required contexts count; a local test run is not a check.
- `evidence_url` is a path on this forge or null. A check's external log link
  is never returned.
