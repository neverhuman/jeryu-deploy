# Pull request review bases

On the GitHub-compatible create edge with a Git backend, a request that omits
`base_sha` must name a base branch that resolves to a real commit. A missing
branch or an unavailable base lookup returns 422 with
`base_sha must resolve to a 40-hex commit`. The forge creates no pull request
in that case. Create the intended base branch and retry; the forge never
substitutes the feature head as a review base.

One exception: the repository's own default branch while it does not exist.
An empty repository's default branch refuses pushes, so its first content
arrives as a bootstrap pull request (a root commit on another branch, opened
against the unborn default). That create is accepted and recorded with no
review base; merging it, with the usual approvals and gates, creates the
default branch at the reviewed head. Git cannot tell an unborn default branch
from one that was deleted, so the exception also covers a deleted default
branch; deleting it takes a repository admin, and git refuses to delete the
branch HEAD names. A missing branch that is not the default is still refused.

The policy deliberately permits reviewed recovery of a deleted default branch.
It does not require proof that the branch never had commits: imported Git data
and deleted refs cannot supply that proof. Absence must still be read
successfully, and the ordinary merge permissions, approvals and gates apply.

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
