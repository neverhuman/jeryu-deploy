# 0012. Authenticate auditor anchor rotation and qualify the bootstrap

Status: Accepted
Date: 2026-10-14
Supersedes: 0011
Superseded-by: none

## Context

ADR 0011 chose centrally approved releases, snapshot-bound jobs, manifest-only
activation and rollback, staged anchor rotation without a client prompt, and
persistent tickets in migration step 2. Its single-valued trust example could
not express add/activate/retire, and its refusal language contradicted the
recorded rotation decision. A snapshot's claim that a key was added in order
cannot authenticate that key to a client which trusts only an older key.
PR #359 also identified a bootstrap and migration mismatch. Rewriting the
accepted record's normative sentences would erase the decision history.

The released 1.7.4 diff auditor omitted control files and produced false caps.
Tool #29 qualifies 1.7.5 on full and diff parity and pins it under today's
schema. That pin does not implement schema 2, persistent bindings, multi-slot
installation, or this record's proposed rotation protocol.

## Decision

ADR 0011's contracts, ownership, migration order and six owner decisions are
incorporated by reference, with the following replacements. This record
supersedes their conflicting sentences and examples. All schema 2 routes,
fields and behavior remain proposed until their implementation PRs land;
this documentation change activates no runtime behavior or trust anchor.

### Authenticate manifest-only anchor rotation

Rotation remains add, activate, retire through protected tool-manifest PRs,
without a deploy acknowledgement or client prompt. Adding an anchor requires
cryptographic continuity from an anchor already trusted by the installation.
TLS, an access token, manifest membership, snapshot order, and a signature by
the proposed new key are insufficient to establish that continuity.

Schema 2 replaces the singular trust signer/cosign fields with immutable
`[[jankurai.trust.anchors]]` entries. Each has `id`, `release_repo`,
`release_signer_sha256`, `cosign_sha256`, and `state` (`staged`, `active`,
`retired`). An approved release names `anchor_id`; its `signing_identity`
must equal that entry's signer digest. Historical anchors are retained for
verification and reproduction. There is exactly one active anchor for new
release activation; staged anchors cannot authorize proof jobs.

Each `[[jankurai.trust.rotations]]` entry has `from_anchor_id`, `to_anchor_id`,
`sequence`, `payload_sha256`, and an offline cosign signature bundle. The signed
payload is UTF-8 JSON canonicalized with
[RFC 8785](https://www.rfc-editor.org/rfc/rfc8785), with these exact fields:
`domain` (`jeryu.jankurai-anchor-transition/v1`), `installation_id`,
`sequence` (a decimal string), `previous_transition_sha256` (or the installed
bootstrap checkpoint), `from_anchor_id`, `to_anchor_id`, `release_repo`,
`release_signer_sha256`, and `cosign_sha256`. Its SHA-256 must equal
`payload_sha256`; every target field must equal the immutable target entry.
The signature is verified with the already trusted source key and its already
pinned cosign. A replacement cosign is digest-checked before it is executed.
The installation id is fixed at enrollment and cannot come from the response
being verified. Sequence numbers strictly increase and the previous digest
must match the installation's last accepted checkpoint. Clients persist that
checkpoint atomically with the accepted anchor set; the forge enforces the
same transition rules before admitting a snapshot.

An installation starting from an older recorded checkpoint verifies every
link from that checkpoint. A disconnected, missing, reordered, replayed or
self-signed transition fails closed. Serving history from the same forge does
not substitute for signature verification. An added target is staged first;
a later protected snapshot may activate it only after continuity and release
qualification/readiness pass. Retiring the old anchor is a later transition,
never removal of historical verification material. The initial anchor and
checkpoint come from rendered protected source or enrollment, never from the
untrusted manifest response. With no valid continuity proof, keep the last
valid authority and report the failure; do not invent an emergency bypass.

### Bootstrap a release that can govern the fleet

The schema 2 bootstrap has exactly one active release with recorded full and
diff parity, including the exact no-base full invocation, and host/image
readiness for every listed artifact. A failed or unfinished candidate cannot
be active. 1.7.4 is excluded by its diff regression; the symbolic 1.7.4 active
examples in ADR 0011 are historical illustrations, not bootstrap instructions.
The implementation chooses and records the exact qualified tuple and evidence
in its PR; 1.7.5 is eligible only when all those readiness conditions hold.

Migration step 2 explicitly includes durable ticket storage and replay,
transactional snapshot/job bindings, and a canonical render of the compiled
forge identity to that same bootstrap entry. It must ship those together.
A restart preserves job id, head, base, policy and auditor snapshot, lease and
purpose; it cannot silently bind existing work to the new active release.
The legacy cross-check remains until step 4. Manifest-only patch activation
is an acceptance check after migration step 5 removes consumer patch pins,
not a claim about the temporary migration steps. Until then each step retains
its required source renders and deployed-identity reconciliation.

### Required implementation evidence

The implementing PRs must test a valid old-key-signed rotation and reject an
injected additional key, new-key self-signature, altered target/cosign digest,
wrong installation, missing/reordered link, replay and checkpoint rollback.
Tests must prove the verifier never executes an unverified replacement cosign,
that staged/retired anchors cannot govern new proof jobs, and that historical
reproduction retains its original verification chain. Restart tests must
recover queued and claimed bindings byte-for-byte across snapshot activation.
Bootstrap tests must reject unqualified full/diff/no-base or unready artifacts
and refuse an identity mismatch between compiled compatibility code and the
bootstrap snapshot. Existing floor, caps, independent-scorer and exact-head
requirements remain mandatory.

## Consequences

Manifest-only rotation needs an old trusted signer to attest the replacement
trust tuple. The release process must produce that proof and retain public
verification material; losing all trusted signing capability stalls rotation.
No new interactive approval step is added. A snapshot alone cannot authorize
its own key. Implementers now have an explicit multi-anchor representation,
continuity rule, bootstrap bar and persistent-job migration requirement.

The ADR numbering/header test validates record structure and paired
supersession only. It does not validate this security design; independent
source review and the implementation evidence above are still required.
