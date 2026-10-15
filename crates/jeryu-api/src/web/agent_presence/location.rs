//! Optional, private session location and trusted worker association.
use serde::{Deserialize, Serialize};

use super::types::{SHORT_MAX, optional};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Location {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

impl Location {
    pub(crate) fn check(mut self) -> Result<Self, String> {
        for (name, path) in [
            ("session_path", &mut self.session_path),
            ("cwd", &mut self.cwd),
            ("checkout_path", &mut self.checkout_path),
        ] {
            *path = optional(&format!("location.{name}"), path.take(), 4096)?;
            if path.as_ref().is_some_and(|p| !p.starts_with('/')) {
                return Err(format!("location.{name} must be an absolute path"));
            }
        }
        self.screen = optional("location.screen", self.screen, 200)?;
        if self
            .checkout_kind
            .as_deref()
            .is_some_and(|v| !matches!(v, "clone" | "worktree"))
        {
            return Err("location.checkout_kind must be clone or worktree".into());
        }
        if self.head.as_ref().is_some_and(|v| {
            !matches!(v.len(), 40 | 64) || !v.bytes().all(|c| c.is_ascii_hexdigit())
        }) {
            return Err("location.head must be a full hexadecimal commit id".into());
        }
        if self.pid == Some(0) {
            return Err("location.pid must be positive".into());
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Worker {
    pub family: String,
    pub slot: String,
}

impl Worker {
    pub(crate) fn check(self) -> Result<Self, String> {
        let check = |name, value| {
            let value = optional(name, Some(value), SHORT_MAX)?
                .ok_or_else(|| format!("{name} must not be empty"))?;
            if !value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            {
                return Err(format!(
                    "{name} must contain only letters, digits, dots, underscores or hyphens"
                ));
            }
            Ok(value)
        };
        Ok(Self {
            family: check("worker.family", self.family)?,
            slot: check("worker.slot", self.slot)?,
        })
    }
}
