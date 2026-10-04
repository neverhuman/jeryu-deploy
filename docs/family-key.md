# One family key

A product family has one key across the whole API: a lowercase slug with no
suffix (`veox`, `jekko`). A family's split manifest and its todo queue spell
the same family with a `-split` suffix, because that is the name of the
directory its repositories are split into. Every endpoint that takes a family
accepts either spelling and answers with the canonical key.

| Endpoint | Takes | Returns |
| --- | --- | --- |
| `GET /api/v1/repos` | `?family=` | `repositories[].family`, `facets.families` |
| `GET /api/v1/attention` | `?family=` | `items[].family`, `items[].family_label` |
| `GET /api/v1/events` | `?family=` | `events[].family`, `events[].family_label` |
| `GET /api/v1/shift/todos` | `?family=`, body `family` | `todos[].family`, `todos[].family_label` |
| `GET /api/v1/shift/shifts` | `?family=` | `shifts[].family`, `shifts[].family_label` |
| `GET /api/v1/shift/families` | — | `families[].name`, `families[].label` |
| `GET|PUT /api/v1/release-board/:family` | path, body `family` | `family`, `family_label` |
| `POST /api/v1/shift/todos/:family/:id/action`, `POST /api/v1/shift/shifts/:family/pr` | path | — |

Rules a client can rely on:

- **The key is canonical.** `?family=veox-split` and `?family=veox` name the
  same family and every answer says `veox`. A client never strips a suffix.
- **The label is for reading.** `family_label` (`label` on
  `GET /api/v1/shift/families`) is what a reader is shown. Today a family's
  key is also its name, so the two match; render the label anyway, so a family
  whose name is not its key needs no client change. The repositories list is a
  jeryu-core contract type and carries the key alone; its label is the key.
- **A family nobody hosts is refused.** The answer is the typed
  `family_unknown` error (422, like every unreadable request; see
  `docs/errors.md`), never an empty 200, so a typo'd filter cannot read as
  "nothing to do". The refusal names the families the forge knows: the
  families of the repositories it hosts, of the queues it serves, of the
  release boards collectors posted, and of the events in the pipeline log.
- **Writes are canonical too.** `PATCH /api/v1/repos/:id` with
  `{"family": "veox-split"}` stores `veox`; a board PUT under either spelling
  is stored, and answered, under the key.

The server side is `crates/jeryu-api/src/web/family.rs`.
