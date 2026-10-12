#!/bin/bash
# logs must redact jbk_, access tokens, and refresh tokens
# Re-run with the same key keeps the cached refresh token and does not print the key.
# A different key replaces enrollment.key and clears the cached refresh token.
set -euo pipefail
umask 077
FORGE=@@FORGE@@
SLUG=@@SLUG@@
KIND=@@KIND@@
LABEL=@@LABEL@@
ENROLLMENT_KEY=@@ENROLLMENT@@
DIR="${HOME}/.jeryu/bots/${SLUG}"
mkdir -p "$DIR/gh"
chmod 700 "$DIR" "$DIR/gh"
if [ -n "$ENROLLMENT_KEY" ]; then
  current=""
  if [ -f "$DIR/enrollment.key" ]; then
    current=$(cat "$DIR/enrollment.key")
  fi
  if [ "$current" != "$ENROLLMENT_KEY" ]; then
    printf '%s' "$ENROLLMENT_KEY" > "$DIR/enrollment.key.new"
    chmod 600 "$DIR/enrollment.key.new"
    mv "$DIR/enrollment.key.new" "$DIR/enrollment.key"
    rm -f "$DIR/refresh.token" "$DIR/access.token"
  fi
elif [ ! -s "$DIR/enrollment.key" ]; then
  printf '%s\n' "enrollment key missing; set BOT_ENROLLMENT_KEY or pass a key file" >&2
  exit 1
fi
if [ ! -s "$DIR/local.secret" ]; then
  python3 - "$DIR/local.secret" <<'SECRET_PY'
import secrets, sys
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    handle.write(secrets.token_hex(16))
SECRET_PY
  chmod 600 "$DIR/local.secret"
fi
export JERYU_BOT_FORGE="$FORGE"
export JERYU_BOT_DIR="$DIR"
export JERYU_GH_DIR="$DIR/gh"
export JERYU_MERGE_TOKEN="$DIR/merge-token"
HOST_KEY=${FORGE#*://}
HOST_KEY=${HOST_KEY%%/*}
HOST_KEY=${HOST_KEY%%\?*}
export JERYU_HOST_KEY="$HOST_KEY"
cat > "$DIR/broker.py" <<'BROKER'
#!/usr/bin/env python3
# logs must redact jbk_, access tokens, and refresh tokens
import fcntl, hmac, http.client, json, os, sys, threading, time, urllib.error, urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

forge = os.environ["JERYU_BOT_FORGE"].rstrip("/")
directory = os.environ["JERYU_BOT_DIR"]
gh_dir = os.environ["JERYU_GH_DIR"]
host_key = os.environ["JERYU_HOST_KEY"]
merge_path = os.environ["JERYU_MERGE_TOKEN"]

def audience(url):
    host = urlparse(url).hostname or ""
    if host in ("127.0.0.1", "localhost", "::1"):
        return "http://" + host
    return "https://" + host

aud = audience(forge)

def write_mode(path, value):
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, mode=0o700, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.write(fd, value.encode())
    os.close(fd)

def write_secret(name, value):
    write_mode(os.path.join(directory, name), value)

def publish(access):
    write_secret("access.token", access)
    write_mode(merge_path, access)
    text = (
        "%s:\n    oauth_token: %s\n    git_protocol: https\n    users:\n"
        "        jeryu:\n            oauth_token: %s\n    user: jeryu\n"
        % (host_key, access, access)
    )
    write_mode(os.path.join(gh_dir, "hosts.yml"), text)

class ExchangeError(Exception):
    def __init__(self, code):
        self.code = code

def post(path, payload):
    data = json.dumps(payload).encode()
    req = urllib.request.Request(
        forge + path,
        data=data,
        headers={"Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.load(resp)
    except urllib.error.HTTPError as err:
        err.read()
        raise ExchangeError(err.code)

def read_named(name):
    path = os.path.join(directory, name)
    if not os.path.isfile(path) or os.path.getsize(path) == 0:
        return None
    with open(path, "r", encoding="utf-8") as handle:
        return handle.read().strip()

def exchange():
    refresh = read_named("refresh.token")
    key = read_named("enrollment.key")
    body = None
    if refresh:
        try:
            body = post("/api/v1/bot/token/refresh", {"refresh_token": refresh, "aud": aud})
        except ExchangeError as err:
            if err.code != 401 or not key:
                sys.stderr.write("token refresh failed\n")
                sys.exit(1)
    if body is None:
        if not key:
            sys.stderr.write("enrollment key missing\n")
            sys.exit(1)
        try:
            body = post("/api/v1/bot/token", {"enrollment_key": key, "aud": aud})
        except ExchangeError:
            sys.stderr.write("token exchange failed\n")
            sys.exit(1)
    write_secret("refresh.token", body["refresh_token"])
    publish(body["access_token"])
    return body["access_token"]

# A rotated refresh token presented twice revokes the whole family, so only
# one refresh may run at a time: a thread lock inside the proxy, and a file
# lock against `refresh-clients` running beside it.
refresh_lock = threading.Lock()

def access_age():
    path = os.path.join(directory, "access.token")
    if not os.path.isfile(path) or os.path.getsize(path) == 0:
        return None
    return time.time() - os.path.getmtime(path)

def fresh_access(max_age=12 * 60):
    with refresh_lock:
        lock_fd = os.open(os.path.join(directory, "refresh.lock"), os.O_WRONLY | os.O_CREAT, 0o600)
        try:
            fcntl.flock(lock_fd, fcntl.LOCK_EX)
            age = access_age()
            if age is not None and age < max_age:
                with open(os.path.join(directory, "access.token"), "r", encoding="utf-8") as handle:
                    return handle.read().strip()
            return exchange()
        finally:
            os.close(lock_fd)

# Access tokens live 15 minutes and gh and git read them from disk, so keep
# them fresh even when no MCP traffic passes through the proxy.
def keep_fresh():
    while True:
        time.sleep(60)
        try:
            fresh_access(max_age=10 * 60)
        except BaseException as err:  # exchange() exits on failure; keep serving
            sys.stderr.write("background refresh failed: %s\n" % type(err).__name__)

def whoami(access):
    req = urllib.request.Request(
        forge + "/api/v1/bot/whoami",
        headers={"Authorization": "Bearer " + access},
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        who = json.load(resp)
    reach = who.get("reach") or {}
    mode = reach.get("mode") if isinstance(reach, dict) else "unknown"
    line = "whoami: %s %s %s\n" % (who.get("kind"), who.get("slug"), mode)
    write_secret("whoami.line", line)

# MCP routing headers. Clients that negotiate an older protocol version do
# not send Mcp-Method or Mcp-Name, and the forge requires them, so the proxy
# fills them from the JSON-RPC body it is forwarding. MCP-Protocol-Version is
# filled from the version the forge agreed at initialize. Every value is copied
# from the request or the forge's own answer, so the forge's check is unchanged.
NAME_PARAM = {"tools/call": "name", "prompts/get": "name", "resources/read": "uri"}
negotiated = {}
negotiated_lock = threading.Lock()

def routing_headers(payload, headers):
    filled = {}
    try:
        message = json.loads(payload) if payload else None
    except ValueError:
        message = None
    if not isinstance(message, dict):
        return filled
    method = message.get("method")
    params = message.get("params") if isinstance(message.get("params"), dict) else {}
    if isinstance(method, str) and method and not headers.get("Mcp-Method"):
        filled["Mcp-Method"] = method
    key = NAME_PARAM.get(method)
    if key and isinstance(params.get(key), str) and not headers.get("Mcp-Name"):
        filled["Mcp-Name"] = params[key]
    if not headers.get("MCP-Protocol-Version"):
        if method == "initialize" and isinstance(params.get("protocolVersion"), str):
            filled["MCP-Protocol-Version"] = params["protocolVersion"]
        else:
            with negotiated_lock:
                version = negotiated.get(headers.get("Mcp-Session-Id") or "")
            if version:
                filled["MCP-Protocol-Version"] = version
    return filled

def remember_version(payload, session, data):
    try:
        request = json.loads(payload) if payload else None
        answer = json.loads(data) if data else None
    except ValueError:
        return
    if not (isinstance(request, dict) and request.get("method") == "initialize"):
        return
    result = answer.get("result") if isinstance(answer, dict) else None
    version = result.get("protocolVersion") if isinstance(result, dict) else None
    if session and isinstance(version, str):
        with negotiated_lock:
            negotiated[session] = version

FORWARD_HEADERS = (
    "Content-Type",
    "Accept",
    "Mcp-Method",
    "Mcp-Name",
    "Mcp-Session-Id",
    "MCP-Protocol-Version",
)

class Proxy(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        text = fmt % args
        try:
            text = text.replace(self.local_secret(), "<local-secret>")
        except OSError:
            text = "redacted"
        lowered = text.lower()
        if "jbk_" in lowered or "jbr_" in lowered or "bearer" in lowered or "eyj" in lowered:
            text = "redacted"
        sys.stderr.write(text + "\n")

    def reject(self, code):
        self.send_response(code)
        self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()

    def local_secret(self):
        with open(os.path.join(directory, "local.secret"), "r", encoding="utf-8") as handle:
            return handle.read().strip()

    def authorized(self):
        secret = self.local_secret()
        path = self.path.split("?", 1)[0]
        prefix = "/mcp/" + secret
        if path == prefix or path.startswith(prefix + "/"):
            return True
        header = self.headers.get("Authorization") or ""
        if header.startswith("Bearer "):
            presented = header[7:].strip()
            if hmac.compare_digest(presented, secret):
                return True
        return False

    def host_ok(self):
        host = self.headers.get("Host") or ""
        port = str(self.server.server_address[1])
        return host == "127.0.0.1:" + port or host == "localhost:" + port

    def handle_one(self):
        if self.headers.get("Origin"):
            self.reject(403)
            return
        if not self.host_ok():
            self.reject(403)
            return
        if not self.authorized():
            self.reject(401)
            return
        try:
            access = fresh_access()
        except (
            ExchangeError,
            urllib.error.URLError,
            http.client.HTTPException,
            OSError,
            ValueError,
            KeyError,
            TypeError,
        ):
            self.reject(502)
            return
        length = int(self.headers.get("Content-Length", "0") or 0)
        payload = self.rfile.read(length) if length else None
        path = self.path.split("?", 1)[0]
        secret = self.local_secret()
        prefix = "/mcp/" + secret
        if path == prefix:
            suffix = ""
        elif path.startswith(prefix + "/"):
            suffix = path[len(prefix):]
        else:
            suffix = ""
        url = forge + "/mcp/bot" + suffix
        req = urllib.request.Request(url, data=payload, method=self.command)
        req.add_header("Authorization", "Bearer " + access)
        for name in FORWARD_HEADERS:
            value = self.headers.get(name)
            if value:
                req.add_header(name, value)
        for name, value in routing_headers(payload, self.headers).items():
            req.add_header(name, value)
        try:
            resp = urllib.request.urlopen(req, timeout=120)
            code = resp.status
            out_type = resp.headers.get("Content-Type", "application/octet-stream")
            session = resp.headers.get("Mcp-Session-Id")
            streaming = "text/event-stream" in out_type
            if streaming:
                # No length is known for an event stream, so end it by closing.
                self.close_connection = True
                self.send_response(code)
                self.send_header("Content-Type", out_type)
                self.send_header("Cache-Control", "no-cache")
                self.send_header("Connection", "close")
                if session:
                    self.send_header("Mcp-Session-Id", session)
                self.end_headers()
                while True:
                    chunk = resp.read(1024)
                    if not chunk:
                        break
                    self.wfile.write(chunk)
                    self.wfile.flush()
                return
            data = resp.read()
            remember_version(payload, session, data)
        except urllib.error.HTTPError as err:
            code = err.code
            out_type = err.headers.get("Content-Type", "application/octet-stream") if err.headers else "application/octet-stream"
            session = err.headers.get("Mcp-Session-Id") if err.headers else None
            data = err.read()
        self.send_response(code)
        self.send_header("Content-Type", out_type)
        if session:
            self.send_header("Mcp-Session-Id", session)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        self.handle_one()

    def do_POST(self):
        self.handle_one()

    def do_DELETE(self):
        self.handle_one()

def serve():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    port = server.server_address[1]
    write_mode(os.path.join(directory, "port"), str(port))
    threading.Thread(target=keep_fresh, daemon=True).start()
    server.serve_forever()

def main():
    command = sys.argv[1] if len(sys.argv) > 1 else ""
    if command == "exchange":
        whoami(fresh_access(max_age=0))
    elif command == "refresh-clients":
        fresh_access(max_age=0)
    elif command == "serve":
        serve()
    else:
        sys.stderr.write("unknown broker command\n")
        sys.exit(2)

if __name__ == "__main__":
    main()
BROKER
chmod 700 "$DIR/broker.py"
python3 "$DIR/broker.py" exchange
reuse=0
if [ -f "$DIR/port" ] && [ -f "$DIR/proxy.pid" ]; then
  pid=$(tr -d '[:space:]' < "$DIR/proxy.pid")
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    cmd=$(ps -p "$pid" -o command= 2>/dev/null || true)
    case "$cmd" in
      *"${DIR}/broker.py"*) reuse=1 ;;
    esac
  fi
fi
if [ "$reuse" != 1 ]; then
  rm -f "$DIR/port"
  nohup python3 "$DIR/broker.py" serve >>"$DIR/proxy.log" 2>&1 &
  echo $! > "$DIR/proxy.pid"
  ready=0
  for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    if [ -s "$DIR/port" ]; then
      ready=1
      break
    fi
    sleep 0.1
  done
  if [ "$ready" != 1 ]; then
    printf '%s\n' "proxy did not bind a loopback port" >&2
    exit 1
  fi
fi
PORT=$(tr -d '[:space:]' < "$DIR/port")
SECRET=$(cat "$DIR/local.secret")
exports=$(printf 'export GH_CONFIG_DIR=%q\nexport JERYU_MERGE_TOKEN=%q\n' "$DIR/gh" "$DIR/merge-token")
printf '%s\n' "$exports" > "$DIR/client.env"
chmod 600 "$DIR/client.env"
printf '%s\n' "$exports" >&2
printf '%s\n' "Refresh gh and git without MCP traffic: python3 \"$DIR/broker.py\" refresh-clients" >&2
if [ "$KIND" != "muse" ]; then
  # Grok and every other kind: an mcpServers entry any MCP client can read.
  if [ "$KIND" = "grok" ]; then
    echo "warning: every Grokbot on one account shares this computer, its files, and its credentials" >&2
  fi
  printf '%s\n' "{\"mcpServers\":{\"jeryu\":{\"url\":\"http://127.0.0.1:${PORT}/mcp/${SECRET}\"}}}"
else
  cat > "$DIR/jeryu-bot.env" <<EOF
JERYU_FORGE_URL=${FORGE}
JERYU_BOT_AUDIENCE=${FORGE}
JERYU_BOT_PROXY=http://127.0.0.1:${PORT}/mcp/${SECRET}
JERYU_BOT_SLUG=${SLUG}
JERYU_BOT_KIND=${LABEL}
GH_CONFIG_DIR=${DIR}/gh
JERYU_MERGE_TOKEN=${DIR}/merge-token
EOF
  chmod 600 "$DIR/jeryu-bot.env"
  cat <<STUB
# Musebot skill stub. Read the env file this Musebot wrote. Do not look for a Muse on-disk skill directory.
#   set -a; . "\$HOME/.jeryu/bots/${SLUG}/jeryu-bot.env"; set +a
# The loopback proxy at \$JERYU_BOT_PROXY attaches a fresh access token to /mcp/bot.
# Refresh gh and git without MCP traffic: python3 "\$HOME/.jeryu/bots/${SLUG}/broker.py" refresh-clients
STUB
fi
cat "$DIR/whoami.line"
