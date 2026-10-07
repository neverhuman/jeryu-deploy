//! The setup script keeps the operator's gh config and merge token, and each
//! slug gets its own authenticated loopback proxy.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::render_bot_setup_script;

const OPERATOR_HOSTS: &str = "github.com:\n    oauth_token: human-github-token\n    user: human\n";
const OPERATOR_MERGE: &str = "human-merge-token\n";

struct Cleanup {
    mock: Option<Child>,
    home: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(child) = self.mock.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let bots = self.home.join(".jeryu/bots");
        if let Ok(entries) = fs::read_dir(&bots) {
            for entry in entries.flatten() {
                if let Ok(text) = fs::read_to_string(entry.path().join("proxy.pid"))
                    && let Ok(pid) = text.trim().parse::<i32>()
                    && pid > 1
                {
                    let _ = Command::new("kill").arg(pid.to_string()).status();
                }
            }
        }
    }
}

#[test]
fn setup_script_isolates_credentials_and_proxy_identity() {
    let home = tempfile::tempdir().expect("home");
    let home_path = home.path().to_path_buf();
    let log_path = home_path.join("forge.log");
    let mock_path = home_path.join("mock_forge.py");
    fs::write(&mock_path, MOCK_FORGE).expect("mock");
    let mut mock = Command::new("python3")
        .arg(&mock_path)
        .arg(&log_path)
        .env("PYTHONUNBUFFERED", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("mock forge");
    let port = {
        let stdout = mock.stdout.take().expect("mock stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("mock port");
        line.trim().parse::<u16>().expect("port number")
    };
    let mut cleanup = Cleanup {
        mock: Some(mock),
        home: home_path.clone(),
    };
    let forge = format!("http://127.0.0.1:{port}");

    let hosts = home_path.join(".config/gh/hosts.yml");
    let merge = home_path.join(".jeryu/secrets/merge-token");
    fs::create_dir_all(hosts.parent().expect("gh dir")).unwrap();
    fs::create_dir_all(merge.parent().expect("secrets dir")).unwrap();
    fs::write(&hosts, OPERATOR_HOSTS).unwrap();
    fs::write(&merge, OPERATOR_MERGE).unwrap();
    let decoy = home_path.join("decoy-gh");
    fs::create_dir_all(&decoy).unwrap();

    let first_key = "jbk_test.one.secret-one";
    let first = run_setup(&home_path, &decoy, &forge, "grok", "atlas", Some(first_key));
    assert!(
        first.status.success(),
        "first setup failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(fs::read(&hosts).unwrap(), OPERATOR_HOSTS.as_bytes());
    assert_eq!(fs::read(&merge).unwrap(), OPERATOR_MERGE.as_bytes());
    assert!(fs::read_dir(&decoy).unwrap().next().is_none());

    let atlas = home_path.join(".jeryu/bots/atlas");
    let private_hosts = fs::read_to_string(atlas.join("gh/hosts.yml")).unwrap();
    assert!(private_hosts.contains("127.0.0.1"));
    assert!(!private_hosts.contains("github.com"));
    assert_eq!(
        fs::read_to_string(atlas.join("merge-token")).unwrap(),
        "access-from-enroll"
    );
    assert_eq!(
        fs::read_to_string(atlas.join("enrollment.key")).unwrap(),
        first_key
    );
    let port_atlas: u16 = fs::read_to_string(atlas.join("port"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid_atlas = fs::read_to_string(atlas.join("proxy.pid")).unwrap();
    assert_ne!(port_atlas, 8765);
    assert_ne!(port_atlas, 8766);
    let secret = fs::read_to_string(atlas.join("local.secret")).unwrap();
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains(&format!("http://127.0.0.1:{port_atlas}/mcp/{secret}")));
    assert!(String::from_utf8_lossy(&first.stderr).contains("GH_CONFIG_DIR"));

    assert!(
        status_line(port_atlas, &format!("127.0.0.1:{port_atlas}"), "/mcp", "")
            .starts_with("HTTP/1.1 401")
    );
    assert!(
        status_line(port_atlas, "evil.example", &format!("/mcp/{secret}"), "")
            .starts_with("HTTP/1.1 403")
    );
    assert!(
        status_line(
            port_atlas,
            &format!("127.0.0.1:{port_atlas}"),
            &format!("/mcp/{secret}"),
            "Origin: http://evil.example\r\n"
        )
        .starts_with("HTTP/1.1 403")
    );
    let ok = status_line(
        port_atlas,
        &format!("127.0.0.1:{port_atlas}"),
        &format!("/mcp/{secret}"),
        "Mcp-Method: initialize\r\n",
    );
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert!(ok.contains("Mcp-Session-Id"), "{ok}");

    let again = run_setup(&home_path, &decoy, &forge, "grok", "atlas", Some(first_key));
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(
        fs::read_to_string(atlas.join("proxy.pid")).unwrap(),
        pid_atlas
    );
    assert_eq!(
        fs::read_to_string(atlas.join("port"))
            .unwrap()
            .trim()
            .parse::<u16>()
            .unwrap(),
        port_atlas
    );

    fs::write(atlas.join("refresh.token"), "jbr_expired").unwrap();
    let refreshed = Command::new("python3")
        .arg(atlas.join("broker.py"))
        .arg("refresh-clients")
        .env("HOME", &home_path)
        .env("JERYU_BOT_FORGE", &forge)
        .env("JERYU_BOT_DIR", &atlas)
        .env("JERYU_GH_DIR", atlas.join("gh"))
        .env("JERYU_HOST_KEY", format!("127.0.0.1:{port}"))
        .env("JERYU_MERGE_TOKEN", atlas.join("merge-token"))
        .output()
        .unwrap();
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let forge_log = fs::read_to_string(&log_path).unwrap();
    assert!(forge_log.contains("refresh jbr_expired"));
    assert!(forge_log.contains(&format!("enroll {first_key}")));

    let second_key = "jbk_test.two.secret-two";
    let rotated = run_setup(
        &home_path,
        &decoy,
        &forge,
        "grok",
        "atlas",
        Some(second_key),
    );
    assert!(
        rotated.status.success(),
        "{}",
        String::from_utf8_lossy(&rotated.stderr)
    );
    assert_eq!(
        fs::read_to_string(atlas.join("enrollment.key")).unwrap(),
        second_key
    );
    assert!(
        fs::read_to_string(&log_path)
            .unwrap()
            .contains(&format!("enroll {second_key}"))
    );

    let other = run_setup(
        &home_path,
        &decoy,
        &forge,
        "muse",
        "nova",
        Some("jbk_test.nova.secret"),
    );
    assert!(
        other.status.success(),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
    let nova_port: u16 = fs::read_to_string(home_path.join(".jeryu/bots/nova/port"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(nova_port, port_atlas);
    assert_eq!(fs::read(&hosts).unwrap(), OPERATOR_HOSTS.as_bytes());
    assert_eq!(fs::read(&merge).unwrap(), OPERATOR_MERGE.as_bytes());
    let _ = cleanup.mock.take().map(|mut child| {
        let _ = child.kill();
        let _ = child.wait();
    });
}

fn run_setup(
    home: &Path,
    decoy: &Path,
    forge: &str,
    kind: &str,
    slug: &str,
    key: Option<&str>,
) -> std::process::Output {
    let script = render_bot_setup_script(kind, slug, forge, key);
    assert!(!script.contains("8765"), "fixed Grokbot port must be gone");
    assert!(!script.contains("8766"), "fixed Musebot port must be gone");
    assert!(!script.contains(".config/gh"));
    assert!(!script.contains(".jeryu/secrets/merge-token"));
    let path = home.join(format!("setup-{slug}.sh"));
    fs::write(&path, &script).unwrap();
    Command::new("bash")
        .arg(&path)
        .env("HOME", home)
        .env("GH_CONFIG_DIR", decoy)
        .env_remove("BOT_ENROLLMENT_KEY")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("bash")
}

fn status_line(port: u16, host: &str, path: &str, extra: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("proxy");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{extra}\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let mut headers = String::new();
    loop {
        let mut chunk = String::new();
        match reader.read_line(&mut chunk) {
            Ok(0) => break,
            Ok(_) => {
                let done = chunk == "\r\n" || chunk == "\n";
                headers.push_str(&chunk);
                if done {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    headers
}

const MOCK_FORGE: &str = r#"
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

log_path = sys.argv[1]

def note(line):
    with open(log_path, "a", encoding="utf-8") as handle:
        handle.write(line + "\n")

class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        return

    def body(self):
        length = int(self.headers.get("Content-Length", "0") or 0)
        raw = self.rfile.read(length) if length else b""
        if not raw:
            return {}
        return json.loads(raw)

    def send_json(self, code, payload, session=None):
        data = json.dumps(payload).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        if session:
            self.send_header("Mcp-Session-Id", session)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        payload = self.body()
        if self.path == "/api/v1/bot/token/refresh":
            token = payload.get("refresh_token", "")
            note("refresh " + token)
            if token == "jbr_expired":
                self.send_json(401, {"code": "invalid_token"})
                return
            self.send_json(200, {"access_token": "access-from-refresh", "refresh_token": "jbr_new"})
            return
        if self.path == "/api/v1/bot/token":
            note("enroll " + payload.get("enrollment_key", ""))
            self.send_json(200, {"access_token": "access-from-enroll", "refresh_token": "jbr_from_enroll"})
            return
        if self.path.startswith("/mcp/bot"):
            note("mcp")
            method = self.headers.get("Mcp-Method")
            if method != "initialize":
                self.send_json(400, {"error": "missing method"})
                return
            self.send_json(200, {"ok": True}, session="sess-1")
            return
        self.send_json(404, {})

    def do_GET(self):
        if self.path.startswith("/api/v1/bot/whoami"):
            note("whoami")
            self.send_json(200, {"kind": "Grokbot", "slug": "atlas", "reach": {"mode": "general"}})
            return
        if self.path.startswith("/mcp/bot"):
            note("mcp")
            method = self.headers.get("Mcp-Method")
            if method != "initialize":
                self.send_json(400, {"error": "missing method"})
                return
            self.send_json(200, {"ok": True}, session="sess-1")
            return
        self.send_json(404, {})

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
"#;
