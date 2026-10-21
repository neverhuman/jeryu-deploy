#!/usr/bin/env python3
"""Small, deliberately non-approving forge client. See docs/agent-forge.md."""
import argparse
import json
import os
from pathlib import Path
import re
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("forge redirects are refused")


def read_token(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd) as source:
        info = os.fstat(source.fileno())
        if (not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600
                or info.st_uid != os.getuid()):
            raise ValueError("token must be an owned regular file with mode 0600")
        token = source.read(4097).strip()
    if not token or len(token) > 4096 or any(c.isspace() for c in token):
        raise ValueError("invalid token file")
    return token


def positive(value):
    if not re.fullmatch(r"[1-9][0-9]*", value):
        raise argparse.ArgumentTypeError("expected a positive PR number")
    return int(value)


def parser():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--repo", required=True, help="owner/repository")
    sub = p.add_subparsers(dest="action", required=True)
    opened = sub.add_parser("open")
    opened.add_argument("--head", required=True)
    opened.add_argument("--base", required=True)
    opened.add_argument("--title", required=True)
    opened.add_argument("--body-file", required=True, type=Path)
    for action in ("comment", "close-as-replaced", "ready", "reviews"):
        cmd = sub.add_parser(action)
        cmd.add_argument("number", type=positive)
        if action == "comment":
            cmd.add_argument("--body-file", required=True, type=Path)
            cmd.add_argument("--head-sha", required=True)
        if action == "close-as-replaced":
            cmd.add_argument("--replacement", required=True, type=positive)
    return p


def request_spec(args):
    if not re.fullmatch(r"[A-Za-z0-9_-][A-Za-z0-9_.-]*/[A-Za-z0-9_-][A-Za-z0-9_.-]*", args.repo):
        raise ValueError("expected owner/repository")
    root = "/api/v1/repos/" + args.repo
    if args.action == "open":
        return "POST", root.replace("/api/v1/", "/api/v3/") + "/pulls", dict(head=args.head, base=args.base,
            title=args.title, body=args.body_file.read_text(), draft=True)
    pull = root + "/pulls/" + str(args.number)
    if args.action == "comment":
        if not re.fullmatch(r"[0-9a-fA-F]{40}", args.head_sha):
            raise ValueError("expected a 40-hex head SHA")
        return "POST", pull + "/reviews", {
            "verdict": "comment", "expected_head_sha": args.head_sha,
            "body_markdown": args.body_file.read_text(), "thread_comments": []}
    if args.action == "close-as-replaced":
        if args.replacement == args.number:
            raise ValueError("a PR cannot replace itself")
        return "POST", pull + "/close", {"superseded_by": args.replacement}
    if args.action == "ready":
        return "POST", pull + "/ready", {}
    return "GET", pull, None


def main():
    args = parser().parse_args()
    try:
        base = os.environ["JERYU_FORGE_URL"].rstrip("/")
        url = urllib.parse.urlsplit(base)
        if (url.scheme != "https" or not url.hostname or url.username
                or url.password or url.path or url.query or url.fragment):
            raise ValueError("JERYU_FORGE_URL must be an HTTPS origin")
        credentials = Path(os.environ["JERYU_CREDENTIALS_DIR"])
        if not credentials.is_absolute():
            raise ValueError("JERYU_CREDENTIALS_DIR must be absolute")
        method, route, body = request_spec(args)
        token = read_token(credentials / "agent-forge.token")
        req = urllib.request.Request(base + route,
            data=None if body is None else json.dumps(body).encode(),
            headers={"Authorization": "Bearer " + token,
                     "Content-Type": "application/json"}, method=method)
        # One attempt: a failed response may still have created a PR or comment.
        with urllib.request.build_opener(NoRedirect).open(req, timeout=30) as response:
            data = response.read(4 * 1024 * 1024 + 1)
            if len(data) > 4 * 1024 * 1024:
                raise ValueError("forge response exceeds size limit")
            sys.stdout.write(data.decode())
            sys.stdout.write("\n")
        return 0
    except urllib.error.HTTPError as error:
        print(f"forge HTTP {error.code}; inspect live state before retrying", file=sys.stderr)
    except (KeyError, OSError, ValueError, urllib.error.URLError):
        print("forge request failed; check configuration, custody and live state", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
