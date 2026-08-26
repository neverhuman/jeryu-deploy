# Changelog

## jeryu-deploy-v5.0.0-split.4

- Bind pull-request reviews and merge protection to the exact current head via
  immutable Jeryu Core split.5, preserving historical reviews as stale audit
  evidence and applying latest-review-per-reviewer precedence.
- Bind hosted MCP and agent entry points to authenticated principals rather
  than request-supplied actor names.
- Rotate authenticated sessions after password changes so the old auth epoch is
  revoked while the caller receives a fresh cookie and CSRF token.
- Isolate live Git LFS transport tests from user and system Git configuration so
  global filter hooks cannot mutate or falsely fail the disposable repository.

## jeryu-deploy-v5.0.0-split.3

- Decode bounded gzip/x-gzip Git smart-HTTP pack RPC requests before invoking
  Git, reject malformed or stacked encodings, and preserve protocol-v2 headers.
- Govern every active Jankurai consumer on the local-authority 1.6.11 split.2 tag and
  exact binary digest and installation receipt, with physical-file,
  wrong-authority, hostile-substitution, and PATH-neutralization tests plus
  root-broker release-custody controls; retain the 1.6.10 score baseline only as
  non-authoritative history.

## jeryu-deploy-v5.0.0-split.0 - 2026-06-11
- MAJOR: first standalone split-family release; the legacy monorepo
  (/home/ubuntu/jeryu) is deprecated and its drift fully reconciled.

## jeryu-deploy-v4.0.0-split.0

- Initial split-family baseline for `jeryu-deploy`.
