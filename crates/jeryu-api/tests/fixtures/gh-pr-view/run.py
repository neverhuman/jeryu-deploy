"""Run pinned gh through an ephemeral loopback TLS recorder into Axum."""
import http.client
import http.server
import json
import os
import pathlib
import ssl
import subprocess
import sys
import tempfile
import threading

fixture = json.load(sys.stdin)
version = subprocess.run(["gh", "--version"], env={"PATH": os.environ["PATH"], "GH_CONFIG_DIR": sys.argv[2]}, capture_output=True, text=True, check=True).stdout.splitlines()[0]
assert version.startswith("gh version 2.45.0 "), version
with tempfile.TemporaryDirectory(dir=sys.argv[2]) as directory:
    root = pathlib.Path(directory)
    cert, key = root / "cert.pem", root / "key.pem"
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1", "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost", "-keyout", str(key), "-out", str(cert)], check=True, capture_output=True)
    requests = []

    class Proxy(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            length = int(self.headers["Content-Length"])
            assert length < 65536
            body = self.rfile.read(length)
            requests.append({"path": self.path, "body": json.loads(body)})
            conn = http.client.HTTPConnection(sys.argv[1], timeout=10)
            conn.request("POST", self.path, body, {"Content-Type": "application/json", "Authorization": self.headers["Authorization"]})
            response = conn.getresponse()
            data = response.read()
            self.send_response(response.status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            conn.close()

        def log_message(self, *_args):
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Proxy)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    host = f"localhost:{server.server_port}"
    env = {"PATH": os.environ["PATH"], "HOME": str(root), "GH_CONFIG_DIR": str(root), "GH_ENTERPRISE_TOKEN": fixture["token"], "SSL_CERT_FILE": str(cert), "GH_PROMPT_DISABLED": "1", "GH_NO_UPDATE_NOTIFIER": "1", "NO_COLOR": "1"}
    try:
        for expected in fixture["pulls"]:
            result = subprocess.run(["gh", "pr", "view", str(expected["number"]), "--repo", f"{host}/ada/widgets", "--json", "number,title,state,isDraft,headRefOid,url"], env=env, capture_output=True, text=True, timeout=20)
            assert result.returncode == 0, result.stderr
            assert json.loads(result.stdout) == expected, result.stdout
        captured = json.load(open(pathlib.Path(__file__).with_name("request.json")))
        assert len(requests) == len(fixture["pulls"]), requests
        for request, expected in zip(requests, fixture["pulls"]):
            assert request["path"] == captured["path"], request
            assert request["body"]["query"] == captured["body"]["query"], request
            assert request["body"]["variables"] == {"owner": "ada", "repo": "widgets", "pr_number": expected["number"]}, request
        print(f"{version}: {len(requests)} captured PR reads passed")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
