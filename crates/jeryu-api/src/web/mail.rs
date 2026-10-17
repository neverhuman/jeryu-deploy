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
//! Optional, for how a message reads:
//!
//! - `JERYU_MAIL_REPLY_TO`: a plain address replies go to, for example
//!   `help@example.com`, when the sender is not read by a person.
//! - `JERYU_MAIL_FOOTER`: one line of text closing every branded message, for
//!   example `Run by Example Co.`, linked to `JERYU_MAIL_FOOTER_URL` (https)
//!   when that is set too.
//!
//! An invalid optional value is named on one startup line starting
//! `jeryu-api mail:` and ignored; it never stops mail.
//!
//! Nothing here logs a message body or a link: a link in a log file is a
//! password reset anyone with the log can spend. [`OutgoingMail`]'s `Debug`
//! prints the recipient and subject and redacts the body for the same reason.

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use base64::Engine as _;
use uuid::Uuid;

const FROM_ENV: &str = "JERYU_MAIL_FROM";
const BASE_URL_ENV: &str = "JERYU_MAIL_BASE_URL";
const SENDMAIL_ENV: &str = "JERYU_MAIL_SENDMAIL";
const SPOOL_ENV: &str = "JERYU_MAIL_SPOOL";
const REPLY_TO_ENV: &str = "JERYU_MAIL_REPLY_TO";
const FOOTER_ENV: &str = "JERYU_MAIL_FOOTER";
const FOOTER_URL_ENV: &str = "JERYU_MAIL_FOOTER_URL";
const FOOTER_MAX: usize = 200;

/// One message, built and ready to hand to a transport.
#[derive(Clone)]
pub(crate) struct OutgoingMail {
    pub to: String,
    pub subject: String,
    /// The plain-text body; every message has one.
    pub body: String,
    /// An HTML rendering of the same message, sent alongside the text.
    pub html: Option<String>,
}

impl fmt::Debug for OutgoingMail {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutgoingMail")
            .field("to", &self.to)
            .field("subject", &self.subject)
            // The body carries the link, which is the secret itself.
            .field("body", &"[REDACTED]")
            .field("html", &self.html.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

impl OutgoingMail {
    /// The message as RFC 5322 text, with CRLF line endings. With an HTML
    /// rendering it is `multipart/alternative`: the text first, then the HTML
    /// in base64 so no line of it can run past the transport's limit.
    fn render(&self, from: &str, reply_to: Option<&str>, now_ms: i64) -> String {
        let date = chrono::DateTime::from_timestamp_millis(now_ms)
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc2822();
        let mut text = String::new();
        text.push_str(&format!("From: {from}\r\n"));
        text.push_str(&format!("To: {}\r\n", self.to));
        if let Some(reply_to) = reply_to {
            text.push_str(&format!("Reply-To: {reply_to}\r\n"));
        }
        text.push_str(&format!("Subject: {}\r\n", self.subject));
        text.push_str(&format!("Date: {date}\r\n"));
        text.push_str(&format!("Message-ID: <{}@jeryu>\r\n", Uuid::new_v4()));
        text.push_str("MIME-Version: 1.0\r\n");
        text.push_str("Auto-Submitted: auto-generated\r\n");
        let Some(html) = &self.html else {
            text.push_str("Content-Type: text/plain; charset=utf-8\r\n");
            text.push_str("\r\n");
            push_lines(&mut text, &self.body);
            return text;
        };
        let boundary = format!("jeryu-{}", Uuid::new_v4().simple());
        text.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n"
        ));
        text.push_str("\r\n");
        text.push_str(&format!("--{boundary}\r\n"));
        text.push_str("Content-Type: text/plain; charset=utf-8\r\n");
        text.push_str("Content-Transfer-Encoding: 8bit\r\n");
        text.push_str("\r\n");
        push_lines(&mut text, &self.body);
        text.push_str(&format!("--{boundary}\r\n"));
        text.push_str("Content-Type: text/html; charset=utf-8\r\n");
        text.push_str("Content-Transfer-Encoding: base64\r\n");
        text.push_str("\r\n");
        let encoded = base64::engine::general_purpose::STANDARD.encode(html.as_bytes());
        for chunk in encoded.as_bytes().chunks(76) {
            text.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            text.push_str("\r\n");
        }
        text.push_str(&format!("--{boundary}--\r\n"));
        text
    }
}

fn push_lines(text: &mut String, body: &str) {
    for line in body.lines() {
        text.push_str(line);
        text.push_str("\r\n");
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
            .arg("--")
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
    brand: MailBrand,
    transport: Arc<dyn MailTransport>,
}

/// What a branded message needs from the configuration besides its words.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MailBrand {
    /// Where replies go, when the sender is not read by a person.
    pub reply_to: Option<String>,
    /// The closing line, and where it links.
    pub footer: Option<String>,
    pub footer_url: Option<String>,
}

impl MailBrand {
    /// Read and check the optional settings; each bad one is named and dropped.
    fn from_env() -> Self {
        let reply_to = env_value(REPLY_TO_ENV).and_then(|raw| {
            let address = super::account_email::normalize_address(&raw);
            if address.is_none() {
                eprintln!("jeryu-api mail: {REPLY_TO_ENV} is not a plain address; ignored");
            }
            address
        });
        let footer = env_value(FOOTER_ENV).and_then(|raw| {
            let ok = raw.chars().count() <= FOOTER_MAX && !raw.chars().any(char::is_control);
            if !ok {
                eprintln!(
                    "jeryu-api mail: {FOOTER_ENV} must be one line of at most {FOOTER_MAX} characters; ignored"
                );
            }
            ok.then_some(raw)
        });
        let footer_url = env_value(FOOTER_URL_ENV).and_then(|raw| {
            let ok = raw.starts_with("https://")
                && raw.len() > "https://".len()
                && !raw
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>'));
            if !ok {
                eprintln!("jeryu-api mail: {FOOTER_URL_ENV} must be an https URL; ignored");
            }
            ok.then_some(raw)
        });
        Self {
            reply_to,
            footer,
            footer_url,
        }
    }
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
                brand: MailBrand::from_env(),
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
                brand: MailBrand::default(),
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
                brand: MailBrand::default(),
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

    /// The optional reply-to and footer a branded message carries.
    pub(crate) fn brand(&self) -> MailBrand {
        self.inner
            .as_ref()
            .map(|configured| configured.brand.clone())
            .unwrap_or_default()
    }

    /// Test-only: a recording mailer that also carries `brand`.
    #[cfg(test)]
    pub(crate) fn recording_with_brand(brand: MailBrand) -> Self {
        let recording = Arc::new(RecordingTransport::default());
        Self {
            inner: Some(Arc::new(Configured {
                from: "jeryu <no-reply@jeryu.test>".to_string(),
                base_url: "https://jeryu.test".to_string(),
                brand,
                transport: recording.clone(),
            })),
            recording: Some(recording),
        }
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
            let rendered = mail.render(
                &configured.from,
                configured.brand.reply_to.as_deref(),
                now_ms,
            );
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
            html: None,
        };
        let rendered = mail.render("jeryu <no-reply@example.com>", None, 1_700_000_000_000);
        assert!(
            rendered.starts_with("From: jeryu <no-reply@example.com>\r\n"),
            "{rendered}"
        );
        assert!(rendered.contains("To: ada@example.com\r\n"));
        assert!(rendered.contains("Subject: Reset your jeryu password\r\n"));
        assert!(rendered.contains("Auto-Submitted: auto-generated\r\n"));
        assert!(rendered.contains("\r\n\r\nline one\r\nline two\r\n"));
    }

    #[test]
    fn an_html_rendering_goes_alongside_the_text_with_a_reply_to() {
        use base64::Engine as _;
        let mail = OutgoingMail {
            to: "ada@example.com".to_string(),
            subject: "Hello".to_string(),
            body: "plain words".to_string(),
            html: Some(format!("<p>{}</p>", "x".repeat(300))),
        };
        let rendered = mail.render(
            "jeryu <no-reply@example.com>",
            Some("help@example.com"),
            1_700_000_000_000,
        );
        assert!(
            rendered.contains("Reply-To: help@example.com\r\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Content-Type: multipart/alternative; boundary=\"jeryu-"),
            "{rendered}"
        );
        assert!(rendered.contains("Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\nplain words\r\n"), "{rendered}");
        assert!(
            rendered.contains("Content-Transfer-Encoding: base64\r\n"),
            "{rendered}"
        );
        assert!(
            rendered.lines().all(|line| line.len() <= 998),
            "a line ran past 998"
        );
        let encoded: String = rendered
            .split("Content-Transfer-Encoding: base64\r\n\r\n")
            .nth(1)
            .and_then(|rest| rest.split("\r\n--").next())
            .expect("an html part")
            .split("\r\n")
            .collect();
        let html = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64 html");
        assert_eq!(
            String::from_utf8(html).unwrap(),
            format!("<p>{}</p>", "x".repeat(300))
        );
    }

    #[test]
    fn a_plain_message_stays_single_part() {
        let mail = OutgoingMail {
            to: "ada@example.com".to_string(),
            subject: "Hello".to_string(),
            body: "words".to_string(),
            html: None,
        };
        let rendered = mail.render("jeryu <no-reply@example.com>", None, 1_700_000_000_000);
        assert!(!rendered.contains("multipart"), "{rendered}");
        assert!(!rendered.contains("Reply-To"), "{rendered}");
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
                html: None,
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
                html: None,
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

    #[test]
    fn a_recipient_cannot_be_a_sendmail_option() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("mail transport fixture");
        let program = dir.path().join("sendmail");
        std::fs::write(&program,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/argv\"\ncat > \"$(dirname \"$0\")/message\"\n")
            .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let recipient = "-fattack@attacker.test";
        assert_eq!(
            super::super::account_email::normalize_address(recipient).as_deref(),
            Some(recipient)
        );
        let transport = SendmailTransport {
            program: program.to_str().unwrap().to_string(),
            args: vec!["-t".to_string(), "-i".to_string()],
        };
        transport
            .deliver("From: fixture\r\n\r\nbody\r\n", recipient)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("argv")).unwrap(),
            "-t\n-i\n--\n-fattack@attacker.test\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("message")).unwrap(),
            "From: fixture\r\n\r\nbody\r\n"
        );
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
