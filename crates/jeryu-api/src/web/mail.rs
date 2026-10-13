//! Transactional email for the account surface: the one way the forge sends a
//! person a link.
//!
//! The forge carries no SMTP client and gains none here. A host that sends
//! mail already runs something that does — a local MTA, or a submission relay
//! behind one — so a message leaves this process one of two ways, both chosen
//! by configuration and neither of them a network dependency of the API:
//!
//! - `JERYU_MAIL_SENDMAIL`: a sendmail-compatible program the message is piped
//!   to, for example `/usr/sbin/sendmail -t -i`. The program's own queue
//!   retries; this process does not.
//! - `JERYU_MAIL_SPOOL`: a directory one RFC 5322 file is written into per
//!   message, named `<unix-ms>-<uuid>.eml`, for a path unit or cron job on the
//!   host to pick up and submit. Written to `<name>.tmp` and renamed, so a
//!   reader never sees half a message.
//!
//! Configured with neither, the forge sends nothing: `Mailer::is_configured`
//! is false, every account route still answers exactly as it would have (a
//! recovery request that discloses nothing must keep disclosing nothing when
//! the operator has not finished wiring mail), and the refusal is recorded in
//! the audit log instead. `docs/account-email.md` is the operator's half.
//!
//! Required alongside a transport:
//!
//! - `JERYU_MAIL_FROM`: the envelope and header sender, for example
//!   `jeryu <no-reply@example.com>`.
//! - `JERYU_MAIL_BASE_URL`: the public origin links are built on, for example
//!   `https://jeryu.example.com`. A link is useless if it points at the bind
//!   address, so this is not guessed from the listener.
//!
//! Nothing here logs a message body or a link: a link in a log file is a
//! password reset anyone with the log can spend. [`OutgoingMail`]'s `Debug`
//! prints the recipient and subject and redacts the body for the same reason.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use uuid::Uuid;

const FROM_ENV: &str = "JERYU_MAIL_FROM";
const BASE_URL_ENV: &str = "JERYU_MAIL_BASE_URL";
const SENDMAIL_ENV: &str = "JERYU_MAIL_SENDMAIL";
const SPOOL_ENV: &str = "JERYU_MAIL_SPOOL";

/// One message, built and ready to hand to a transport.
#[derive(Clone)]
pub(crate) struct OutgoingMail {
    pub to: String,
    pub subject: String,
    pub body: String,
}

impl fmt::Debug for OutgoingMail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutgoingMail")
            .field("to", &self.to)
            .field("subject", &self.subject)
            // The body carries the link, which is the secret itself.
            .field("body", &"[REDACTED]")
            .finish()
    }
}

impl OutgoingMail {
    /// The message as RFC 5322 text, with CRLF line endings.
    fn render(&self, from: &str, now_ms: i64) -> String {
        let date = chrono::DateTime::from_timestamp_millis(now_ms)
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc2822();
        let mut text = String::new();
        text.push_str(&format!("From: {from}\r\n"));
        text.push_str(&format!("To: {}\r\n", self.to));
        text.push_str(&format!("Subject: {}\r\n", self.subject));
        text.push_str(&format!("Date: {date}\r\n"));
        text.push_str(&format!("Message-ID: <{}@jeryu>\r\n", Uuid::new_v4()));
        text.push_str("MIME-Version: 1.0\r\n");
        text.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        text.push_str("Auto-Submitted: auto-generated\r\n");
        text.push_str("\r\n");
        for line in self.body.lines() {
            text.push_str(line);
            text.push_str("\r\n");
        }
        text
    }
}

/// Where a rendered message goes.
trait MailTransport: Send + Sync {
    fn deliver(&self, rendered: &str, to: &str) -> Result<(), String>;
    /// For the audit detail, so a record says which way the message left.
    fn name(&self) -> &'static str;
}

/// Pipe the message to a sendmail-compatible program.
struct SendmailTransport {
    program: String,
    args: Vec<String>,
}

impl MailTransport for SendmailTransport {
    fn deliver(&self, rendered: &str, to: &str) -> Result<(), String> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .arg(to)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|err| format!("could not run {}: {err}", self.program))?;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| "the mail program took no stdin".to_string())?
            .write_all(rendered.as_bytes())
            .map_err(|err| format!("could not write to {}: {err}", self.program))?;
        let status = child
            .wait()
            .map_err(|err| format!("could not wait for {}: {err}", self.program))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{} exited with {status}", self.program))
        }
    }

    fn name(&self) -> &'static str {
        "sendmail"
    }
}

/// Write one file per message for the host to submit.
struct SpoolTransport {
    dir: PathBuf,
}

impl MailTransport for SpoolTransport {
    fn deliver(&self, rendered: &str, _to: &str) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|err| format!("could not create the mail spool: {err}"))?;
        let name = format!(
            "{}-{}.eml",
            chrono::Utc::now().timestamp_millis(),
            Uuid::new_v4()
        );
        let staged = self.dir.join(format!("{name}.tmp"));
        std::fs::write(&staged, rendered)
            .map_err(|err| format!("could not write the spooled message: {err}"))?;
        std::fs::rename(&staged, self.dir.join(&name))
            .map_err(|err| format!("could not publish the spooled message: {err}"))
    }

    fn name(&self) -> &'static str {
        "spool"
    }
}

/// Keep every message in memory for a test to read back.
#[cfg(test)]
#[derive(Default)]
struct RecordingTransport {
    sent: std::sync::Mutex<Vec<(String, String)>>,
}

#[cfg(test)]
impl MailTransport for RecordingTransport {
    fn deliver(&self, rendered: &str, to: &str) -> Result<(), String> {
        self.sent
            .lock()
            .expect("recording mail mutex poisoned")
            .push((to.to_string(), rendered.to_string()));
        Ok(())
    }

    fn name(&self) -> &'static str {
        "recording"
    }
}

/// How the account routes send mail. Cheap to clone; shares one transport.
#[derive(Clone)]
pub(crate) struct Mailer {
    inner: Option<Arc<Configured>>,
    #[cfg(test)]
    recording: Option<Arc<RecordingTransport>>,
}

struct Configured {
    from: String,
    base_url: String,
    transport: Arc<dyn MailTransport>,
}

impl fmt::Debug for Mailer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Mailer")
            .field("configured", &self.is_configured())
            .finish()
    }
}

impl Mailer {
    /// Read the transport and its addresses once, from the environment.
    ///
    /// A half-configured mailer is an unconfigured one: a transport without a
    /// sender or a base URL would send a message no one can reply to carrying
    /// a link no one can follow.
    pub(crate) fn from_env() -> Self {
        let from = env_value(FROM_ENV);
        let base_url = env_value(BASE_URL_ENV).map(|url| url.trim_end_matches('/').to_string());
        let transport: Option<Arc<dyn MailTransport>> =
            match (env_value(SENDMAIL_ENV), env_value(SPOOL_ENV)) {
                (Some(command), _) => {
                    let mut words = command.split_whitespace().map(str::to_string);
                    words.next().map(|program| {
                        Arc::new(SendmailTransport {
                            program,
                            args: words.collect(),
                        }) as Arc<dyn MailTransport>
                    })
                }
                (None, Some(dir)) => Some(Arc::new(SpoolTransport {
                    dir: PathBuf::from(dir),
                })),
                (None, None) => None,
            };
        let inner = match (from, base_url, transport) {
            (Some(from), Some(base_url), Some(transport)) => Some(Arc::new(Configured {
                from,
                base_url,
                transport,
            })),
            _ => None,
        };
        Self {
            inner,
            #[cfg(test)]
            recording: None,
        }
    }

    /// Test-only: a configured mailer that keeps every message in memory.
    #[cfg(test)]
    pub(crate) fn recording() -> Self {
        let recording = Arc::new(RecordingTransport::default());
        Self {
            inner: Some(Arc::new(Configured {
                from: "jeryu <no-reply@jeryu.test>".to_string(),
                base_url: "https://jeryu.test".to_string(),
                transport: recording.clone(),
            })),
            recording: Some(recording),
        }
    }

    /// Test-only: a configured mailer whose transport refuses every message,
    /// as a host whose sendmail program fails leaves it.
    #[cfg(test)]
    pub(crate) fn failing() -> Self {
        Self {
            inner: Some(Arc::new(Configured {
                from: "jeryu <no-reply@jeryu.test>".to_string(),
                base_url: "https://jeryu.test".to_string(),
                transport: Arc::new(SendmailTransport {
                    program: "/nonexistent/sendmail".to_string(),
                    args: Vec::new(),
                }),
            })),
            recording: None,
        }
    }

    /// Test-only: a mailer with no transport, as an operator who has not
    /// wired mail yet leaves it.
    #[cfg(test)]
    pub(crate) fn unconfigured() -> Self {
        Self {
            inner: None,
            recording: None,
        }
    }

    /// Test-only: every message sent so far, as `(recipient, rendered text)`.
    #[cfg(test)]
    pub(crate) fn sent(&self) -> Vec<(String, String)> {
        self.recording
            .as_ref()
            .map(|recording| {
                recording
                    .sent
                    .lock()
                    .expect("recording mail mutex poisoned")
                    .clone()
            })
            .unwrap_or_default()
    }

    pub(crate) fn is_configured(&self) -> bool {
        self.inner.is_some()
    }

    /// Which transport a message would leave by, for an audit detail.
    pub(crate) fn transport_name(&self) -> &'static str {
        self.inner
            .as_ref()
            .map_or("none", |configured| configured.transport.name())
    }

    /// `<base url>/<path>`, the origin links are built on.
    pub(crate) fn link(&self, path: &str) -> Option<String> {
        let configured = self.inner.as_ref()?;
        Some(format!(
            "{}/{}",
            configured.base_url,
            path.trim_start_matches('/')
        ))
    }

    /// Render and hand `mail` to the transport.
    ///
    /// Both transports block — one spawns a process, the other writes a file —
    /// so the work runs on a blocking thread and the request's thread is not
    /// held while the host's MTA takes the message.
    pub(crate) async fn send(&self, mail: OutgoingMail) -> Result<(), String> {
        let Some(configured) = self.inner.clone() else {
            return Err("mail is not configured".to_string());
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        tokio::task::spawn_blocking(move || {
            let rendered = mail.render(&configured.from, now_ms);
            configured.transport.deliver(&rendered, &mail.to)
        })
        .await
        .map_err(|err| format!("the mail task did not finish: {err}"))?
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_message_carries_the_headers_a_submission_needs() {
        let mail = OutgoingMail {
            to: "ada@example.com".to_string(),
            subject: "Reset your jeryu password".to_string(),
            body: "line one\nline two".to_string(),
        };
        let rendered = mail.render("jeryu <no-reply@example.com>", 1_700_000_000_000);
        assert!(
            rendered.starts_with("From: jeryu <no-reply@example.com>\r\n"),
            "{rendered}"
        );
        assert!(rendered.contains("To: ada@example.com\r\n"));
        assert!(rendered.contains("Subject: Reset your jeryu password\r\n"));
        assert!(rendered.contains("Auto-Submitted: auto-generated\r\n"));
        assert!(rendered.contains("\r\n\r\nline one\r\nline two\r\n"));
    }

    /// A link in a log is a spendable reset, so the body never prints.
    #[test]
    fn debug_redacts_the_body() {
        let printed = format!(
            "{:?}",
            OutgoingMail {
                to: "ada@example.com".to_string(),
                subject: "Reset your jeryu password".to_string(),
                body: "https://jeryu.test/reset-password?token=supersecret".to_string(),
            }
        );
        assert!(printed.contains("ada@example.com"), "{printed}");
        assert!(!printed.contains("supersecret"), "{printed}");
    }

    #[test]
    fn a_mailer_without_a_transport_sends_nothing() {
        let mailer = Mailer::unconfigured();
        assert!(!mailer.is_configured());
        assert_eq!(mailer.transport_name(), "none");
        assert_eq!(mailer.link("verify-email?token=x"), None);
    }

    #[tokio::test]
    async fn the_recording_mailer_keeps_what_it_was_handed() {
        let mailer = Mailer::recording();
        assert_eq!(
            mailer.link("/verify-email?token=x").as_deref(),
            Some("https://jeryu.test/verify-email?token=x")
        );
        mailer
            .send(OutgoingMail {
                to: "ada@example.com".to_string(),
                subject: "hello".to_string(),
                body: "body".to_string(),
            })
            .await
            .expect("the recording transport accepts a message");
        let sent = mailer.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "ada@example.com");
        assert!(sent[0].1.contains("Subject: hello"));
    }

    /// What the operator configures, read the way production reads it. A
    /// transport alone is not a configuration: the sender and the public
    /// origin are both needed before a link is worth sending.
    #[test]
    fn from_env_is_configured_only_when_every_piece_is_set() {
        let pieces = [
            env_value(FROM_ENV),
            env_value(BASE_URL_ENV),
            env_value(SENDMAIL_ENV).or_else(|| env_value(SPOOL_ENV)),
        ];
        let complete = pieces.iter().all(Option::is_some);
        assert_eq!(
            Mailer::from_env().is_configured(),
            complete,
            "from_env disagreed with the environment it read: {pieces:?}"
        );
    }

    /// The sendmail path pipes the whole message and waits for the program.
    #[tokio::test]
    async fn the_sendmail_transport_pipes_the_message_to_its_program() {
        // Stands in for `/usr/sbin/sendmail -t -i`: it reads the whole
        // message from stdin, ignores the recipient argument appended after
        // its own, and exits zero.
        let transport = SendmailTransport {
            program: "sh".to_string(),
            args: vec!["-c".to_string(), "cat > /dev/null".to_string()],
        };
        assert_eq!(transport.name(), "sendmail");
        transport
            .deliver("From: a\r\n\r\nbody\r\n", "ada@example.com")
            .expect("the program accepts the message on stdin");
    }

    #[tokio::test]
    async fn the_spool_transport_publishes_one_whole_file() {
        let dir = tempfile::tempdir().expect("spool dir");
        let transport = SpoolTransport {
            dir: dir.path().join("outgoing"),
        };
        transport
            .deliver("From: a\r\n\r\nbody\r\n", "ada@example.com")
            .expect("spool accepts a message");
        let entries: Vec<_> = std::fs::read_dir(dir.path().join("outgoing"))
            .expect("spool dir reads")
            .map(|entry| {
                entry
                    .expect("spool entry")
                    .file_name()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].ends_with(".eml"), "{entries:?}");
    }
}
