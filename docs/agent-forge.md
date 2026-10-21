# Agent forge identity, phase 0

Claude and Codex sessions and Jarvi share the dedicated `agent-forge` account.
It is a service account (`kind: service`), role `user`, with explicit write
access only to the repositories agents work on. It is never `alton`, `alton2`
or `pragent`. Agents never submit approvals or switch identity to approve.
Human review remains a separate action.

[ADR 0008](adr/0008-team-bots-owned-by-an-organization.md) is the target team-bot
model: organization ownership, stewards and the bot's own non-admin grants.
Phase 0 uses a marked service login and one PAT because that is the owner's
chosen provisioning path. A service login is not an organization-owned bot;
it does not acquire the ADR's stewardship or token lifecycle guarantees. Move
it to that model as a separate change, revoking this PAT after verification.

The operator sets `JERYU_CREDENTIALS_DIR` to an absolute private credentials
directory outside every checkout, and `JERYU_FORGE_URL` to the HTTPS origin.
The sole agent PAT lives at `$JERYU_CREDENTIALS_DIR/agent-forge.token`, owned by
the operator, mode 0600; keep the directory mode 0700. Never copy it into a
repository, command argument, log, prompt, receipt or environment variable.
All sessions read the same file. Rotate by revoking the old PAT and securely
replacing that file; a rejected request requires operator intervention, never
fallback to another account. The script refuses symlink token files and
redirects, and never prints HTTP error bodies or credentials.

## Talking to the forge

Install this section in the operator's host rules as well as using the
repository's [agent instructions](../AGENTS.md). From this checkout:

```sh
python3 scripts/agents/forge.py --repo OWNER/REPO open --head BRANCH --base main --title TITLE --body-file pr.md
python3 scripts/agents/forge.py --repo OWNER/REPO comment 123 --head-sha HEAD_SHA --body-file comment.md
python3 scripts/agents/forge.py --repo OWNER/REPO close-as-replaced 123 --replacement 124
python3 scripts/agents/forge.py --repo OWNER/REPO ready 124
python3 scripts/agents/forge.py --repo OWNER/REPO reviews 124
```

Opening creates a draft. Mark ready only within the operator's authorization.
Closing uses the forge's checked, same-repository replacement action, which
records the pointer and closes together. Reviews are read from PR detail (the `reviews` field). Comments require the
40-hex head SHA you inspected; the server refuses a stale head. There is no approve or merge command.
Opening uses the compatible `/api/v3` edge; the other actions use `/api/v1`.
API permissions still apply, including author/admin checks for marking ready.
The script makes one request with a 30-second timeout and no automatic retry.
After an ambiguous failure, inspect live state before repeating a mutation.
It is the stopgap for the subsequent Rust `jeryu forge pr` workflow.

## Owner provisioning — include these commands in the PR description

The worker must not run these commands. The owner creates the account and PAT.
Export the two configuration variables above, then run the following from a
trusted terminal (Python standard library only). Set the repository list to
the explicit intended grants. The admin PAT is prompted without echo and is
kept in memory. Passwords and session cookies stay in memory too. The new
password must be stored in the operator's password manager for recovery.
Do not rerun account creation after a partial failure: inspect the account,
kind, grants and token list, revoke an unrecorded PAT, and resume the missing
step. The exclusive token-file create prevents replacing an existing token.

```sh
python3 - <<'PY'
import getpass, http.cookiejar, json, os
from pathlib import Path
import urllib.request

base = os.environ['JERYU_FORGE_URL'].rstrip('/')
assert base.startswith('https://')
login = 'agent-forge'
repos = input('Write grants (space-separated owner/repo): ').split()
admin = getpass.getpass('Administrator PAT: ')
password = getpass.getpass('New password (save in password manager): ')
directory = Path(os.environ['JERYU_CREDENTIALS_DIR'])
assert directory.is_absolute() and directory.is_dir()
assert directory.stat().st_mode & 0o777 == 0o700
path = directory / 'agent-forge.token'
assert not path.exists() and not path.is_symlink()
class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args):
        raise RuntimeError('redirect refused')
opener = urllib.request.build_opener(
    NoRedirect, urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
def call(method, route, body, headers):
    request = urllib.request.Request(base + '/api/v1' + route,
        data=json.dumps(body).encode(), method=method,
        headers={'Content-Type': 'application/json', **headers})
    with opener.open(request, timeout=30) as response:
        return json.load(response)
auth = {'Authorization': 'Bearer ' + admin}
created = call('POST', '/admin/users', {'login': login, 'role': 'user'}, auth)
call('PUT', '/admin/users/' + login + '/kind', {'kind': 'service'}, auth)
for repo in repos:
    call('POST', '/admin/repos/' + repo + '/grants/' + login,
         {'access': 'write'}, auth)
session = call('POST', '/auth/login',
    {'login': login, 'password': created['password']}, {})
session = call('POST', '/auth/password',
    {'currentPassword': created['password'], 'newPassword': password},
    {'x-jeryu-csrf': session['csrfToken']})
receipt = call('POST', '/auth/tokens', {'name': 'agent-forge'},
    {'x-jeryu-csrf': session['csrfToken']})
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as target:
    target.write(receipt['token'] + '\n')
    target.flush()
    os.fsync(target.fileno())
print('Saved agent PAT; record token ID for revocation:', receipt['id'])
PY
```

Revoke through `DELETE /api/v1/auth/tokens/{id}` authenticated as this service
account, or use the owner's account management flow. Confirm attribution with
one authorized draft PR and comment, read reviews, mark ready when authorized,
and close it as replaced by an actual successor. Unit transport tests cover
the client offline; they do not provision an account or exercise a live site.
