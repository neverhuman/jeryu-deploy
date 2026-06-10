# jeryu-deploy Agent Instructions

This is a Jeryu split repository seeded from `cbecf7caa0e932c76a341b2521e66e911233860d`.

Before editing, read `README.md`, `agent/owner-map.json`,
`agent/test-map.json`, `agent/generated-zones.toml`,
`agent/proof-lanes.toml`, `agent/audit-policy.toml`, and
`agent/boundaries.toml`.

Keep split `main` clean. Dirty source imports from `/home/ubuntu/jeryu` belong
on `import/dirty-*` branches or as explicit patches after baseline checks pass.

Cross-repo Rust dependencies are pinned Git dependencies using
`*-v4.0.0-split.0` tags. Only `jeryu-deploy` may use local sibling path patches
for split-family development.
