"""Command and transport tests with real gh and isolated configuration."""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

EXTENSION = Path(__file__).resolve().parent


class Contract(unittest.TestCase):
    def setUp(self):
        self.home = tempfile.TemporaryDirectory()
        self.addCleanup(self.home.cleanup)
        self.env = {"PATH": os.environ["PATH"], "HOME": self.home.name,
                    "GH_CONFIG_DIR": self.home.name, "GH_PROMPT_DISABLED": "1",
                    "GH_NO_UPDATE_NOTIFIER": "1"}
        self.requests = []
        self.responses = [(503, {"message": "try later"})]
        self.request_seen = threading.Event()
        test = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                test.requests.append((self.path, self.headers.get("Authorization")))
                code, body = test.responses[min(len(test.requests) - 1, len(test.responses) - 1)]
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps(body).encode())
                test.request_seen.set()

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.host = f"127.0.0.1:{self.server.server_port}"
        Path(self.home.name, "hosts.yml").write_text(
            f"{self.host}:\n    oauth_token: fixture-token\n    user: bob\n")

    def command(self, operation="wait", *extra):
        args = [str(EXTENSION / "gh-jeryu"), "pr", operation, "1", "--repo", "acme/demo",
                "--host", f"http://{self.host}", "--json"]
        if operation == "wait":
            args += ["--target", "ready", "--timeout", "5"]
        return args + list(extra)

    def invoke(self, operation="wait", *extra):
        return subprocess.run(self.command(operation, *extra), env=self.env,
                              capture_output=True, text=True, timeout=10)

    def test_authentication_is_terminal(self):
        for code in (401, 403):
            self.responses = [(code, {"message": "denied"})]
            result = self.invoke()
            self.assertEqual(result.returncode, 4, result)
            self.assertEqual(json.loads(result.stdout)["result"], "authentication_error")
        self.assertEqual(len(self.requests), 2)
        self.assertTrue(all(auth == "token fixture-token" for _, auth in self.requests))

    def test_transient_errors_back_off_until_timeout(self):
        start = time.monotonic()
        result = self.invoke("wait", "--timeout", "1.4")
        self.assertEqual(result.returncode, 3, result)
        self.assertEqual(len(self.requests), 2)
        self.assertGreaterEqual(time.monotonic() - start, 1.4)
        self.assertTrue(all(path == "/api/v1/repos/acme/demo/pulls/1" for path, _ in self.requests))

    def test_cancel_during_backoff(self):
        process = subprocess.Popen(self.command(), env=self.env, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        try:
            self.assertTrue(self.request_seen.wait(5))
            process.send_signal(signal.SIGTERM)
            output, error = process.communicate(timeout=3)
            self.assertEqual(process.returncode, 130, error)
            self.assertEqual(json.loads(output)["result"], "cancelled")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()

    def test_older_server_is_explicit_error(self):
        self.responses = [(200, {"summary": {}})]
        result = self.invoke("status")
        self.assertEqual(result.returncode, 1, result)
        self.assertEqual(json.loads(result.stdout)["result"], "unsupported_response")

    def test_local_install_dispatches_through_gh(self):
        installed = subprocess.run(["gh", "extension", "install", "."], cwd=EXTENSION,
                                   env=self.env, capture_output=True, text=True, timeout=10)
        self.assertEqual(installed.returncode, 0, installed.stderr)
        self.responses = [(401, {"message": "denied"})]
        args = ["gh", "jeryu"] + self.command("status")[1:]
        result = subprocess.run(args, env=self.env, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 4, result)
        self.assertEqual(json.loads(result.stdout)["result"], "authentication_error")


if __name__ == "__main__":
    unittest.main()
