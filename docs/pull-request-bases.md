# Pull request review bases

On the GitHub-compatible create edge with a Git backend, a request that omits
`base_sha` must name a base branch that resolves to a real commit. A missing
branch or an unavailable base lookup returns 422 with
`base_sha must resolve to a 40-hex commit`. The forge creates no pull request
in that case. Create the intended base branch and retry; the forge never
substitutes the feature head as a review base.

One exception: the repository's own default branch before its first commit.
An empty repository's default branch refuses pushes, so its first content
arrives as a bootstrap pull request (a root commit on another branch, opened
against the unborn default). That create is accepted and recorded with no
review base; merging it creates the default branch at the reviewed head. An
unborn branch that is not the default is still refused.

An explicit `base_sha` retains the existing 40-hex validation contract. The
in-memory router without a Git backend retains its existing core behavior.

A pull request opened while its base exists keeps that actual review-base
identity. If the branch is removed later, the existing merge path may recreate
it at the reviewed head after the ordinary approvals and gates pass. This
merge behavior does not make an unresolved base valid for a new create.

Review-catch path comparisons accept hexadecimal commit identities only and
end Git option parsing before supplying revisions. If a revision, repository,
range or event history cannot be read, the forge leaves findings unresolved;
an unavailable read is never evidence that no findings exist.
