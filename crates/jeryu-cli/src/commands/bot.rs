//! `jeryu bot`: print the setup script for one account bot.
//!
//! The script is the same text the enroll response returns. A re-run omits
//! the enrollment key, so the key is not printed again. The key is read from
//! stdin, a file, or `BOT_ENROLLMENT_KEY`, never from a command argument.

use std::io::{Read, Write};

use jeryu_api::web::{bot_kind_is_valid, render_bot_setup_script};

use crate::cli::BotSetupArgs;
use crate::client::{ClientError, ClientResult};
use crate::commands::render;

pub(crate) fn run(json: bool, args: BotSetupArgs, out: &mut dyn Write) -> ClientResult<()> {
    let kind = args.kind.as_str();
    if !bot_kind_is_valid(kind) {
        return Err(ClientError::Invalid(
            "kind must match ^[a-z][a-z0-9-]{0,31}$ (for example grok, muse, or agent)".to_string(),
        ));
    }
    if args.slug.trim().is_empty() {
        return Err(ClientError::Invalid("slug is required".to_string()));
    }
    let enrollment_key = read_enrollment_key(&args)?;
    let script = render_bot_setup_script(
        kind,
        args.slug.trim(),
        &args.forge,
        enrollment_key.as_deref(),
    );
    render(
        out,
        json,
        &serde_json::json!({ "setup_script": script }),
        &script,
    )
}

fn read_enrollment_key(args: &BotSetupArgs) -> ClientResult<Option<String>> {
    if args.enrollment_key_stdin && args.enrollment_key_file.is_some() {
        return Err(ClientError::Invalid(
            "pass the enrollment key by stdin or by file, not both".to_string(),
        ));
    }
    if args.enrollment_key_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|err| ClientError::Io(err.to_string()))?;
        return non_empty_key(buf);
    }
    if let Some(path) = &args.enrollment_key_file {
        let buf = std::fs::read_to_string(path).map_err(|err| {
            ClientError::Io(format!("could not read the enrollment key file: {err}"))
        })?;
        return non_empty_key(buf);
    }
    match std::env::var("BOT_ENROLLMENT_KEY") {
        Ok(value) => non_empty_key(value),
        Err(_) => Ok(None),
    }
}

fn non_empty_key(value: String) -> ClientResult<Option<String>> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        return Err(ClientError::Invalid(
            "the enrollment key was empty".to_string(),
        ));
    }
    Ok(Some(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_key_file_is_embedded_without_an_argv_flag() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("enrollment.key");
        std::fs::write(&path, "jbk_test.file.secret\n").unwrap();
        let args = BotSetupArgs {
            kind: "grok".to_string(),
            slug: "atlas".to_string(),
            forge: "http://127.0.0.1:9".to_string(),
            enrollment_key_file: Some(path),
            enrollment_key_stdin: false,
        };
        let mut out = Vec::new();
        run(false, args, &mut out).expect("script");
        let script = String::from_utf8(out).unwrap();
        assert!(script.contains("jbk_test.file.secret"));
        assert!(!script.contains("--enrollment-key"));
        assert!(!script.contains(".config/gh"));
    }
}
