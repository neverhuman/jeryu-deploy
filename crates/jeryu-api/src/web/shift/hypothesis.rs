//! The expected user result and its explicitly recorded check.
use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use toml_edit::Value;

use super::todo_file::TodoFile;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Hypothesis {
    pub hypothesis: String,
    pub check_after: Option<CheckAfter>,
    pub outcome: Outcome,
    pub outcome_note: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum CheckAfter {
    Days(u32),
    Date(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Met,
    NotMet,
    Unclear,
    #[default]
    NotChecked,
}

impl Hypothesis {
    pub(crate) fn from_file(todo: &TodoFile) -> Result<Self, String> {
        let mut fields = serde_json::Map::new();
        for (key, value) in &todo.extra {
            if ["hypothesis", "check_after", "outcome", "outcome_note"].contains(&key.as_str()) {
                let value = if let Some(text) = value.as_str() {
                    serde_json::json!(text)
                } else if let Some(days) = value.as_integer() {
                    serde_json::json!(days)
                } else {
                    return Err(format!("invalid {key}"));
                };
                fields.insert(key.clone(), value);
            }
        }
        let record: Self = serde_json::from_value(fields.into()).map_err(|e| e.to_string())?;
        record.validate()?;
        Ok(record)
    }

    fn validate(&self) -> Result<(), String> {
        if self.hypothesis.len() > 1000 || self.hypothesis.contains(['\n', '\r']) {
            return Err("hypothesis must be one sentence of at most 1000 bytes".into());
        }
        if self.outcome_note.len() > 16_000 {
            return Err("outcome_note must be at most 16000 bytes".into());
        }
        match &self.check_after {
            Some(CheckAfter::Days(days)) if *days > 3650 => {
                return Err("check_after must be at most 3650 days".into());
            }
            Some(CheckAfter::Date(date)) => {
                NaiveDate::parse_from_str(date, "%Y-%m-%d")
                    .map_err(|_| "check_after must be a YYYY-MM-DD date or days after deploy")?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Preserve unrelated front matter and omit unset fields on older files.
    pub(crate) fn edit(todo: &mut TodoFile, patch: &serde_json::Value) -> Result<(), String> {
        let fields = patch.as_object().ok_or("outcome check needs an object")?;
        if fields.is_empty()
            || fields.keys().any(|key| {
                !["hypothesis", "check_after", "outcome", "outcome_note"].contains(&key.as_str())
            })
        {
            return Err("send hypothesis, check_after, outcome or outcome_note".into());
        }
        let mut record =
            serde_json::to_value(Self::from_file(todo)?).expect("hypothesis serializes");
        for (key, value) in fields {
            record[key] = value.clone();
        }
        let record: Self = serde_json::from_value(record).map_err(|e| e.to_string())?;
        record.validate()?;
        let encoded = serde_json::to_value(record).expect("hypothesis serializes");
        for (key, value) in fields {
            todo.extra.retain(|(name, _)| name != key);
            if !value.is_null() {
                let value = &encoded[key];
                let toml = if let Some(days) = value.as_u64() {
                    Value::from(days as i64)
                } else {
                    Value::from(value.as_str().ok_or("expected a string")?)
                };
                todo.extra.push((key.clone(), toml));
            }
        }
        Ok(())
    }

    pub(crate) fn due_ms(&self, shipped_ms: i64) -> Option<i64> {
        match self.check_after.as_ref()? {
            CheckAfter::Days(days) => shipped_ms.checked_add(i64::from(*days) * 86_400_000),
            CheckAfter::Date(date) => Some(
                NaiveDate::parse_from_str(date, "%Y-%m-%d")
                    .ok()?
                    .and_hms_opt(0, 0, 0)?
                    .and_utc()
                    .timestamp_millis(),
            ),
        }
    }
}

/// Request-local index: one first complete production deployment per todo.
pub(crate) fn deployments(
    changes: &[crate::web::outcomes::project::Change],
) -> BTreeMap<(&str, &str), i64> {
    changes
        .iter()
        .filter_map(|change| {
            Some((
                (change.family.as_deref()?, change.todo_id.as_deref()?),
                deployed_ms(&change.legs)?,
            ))
        })
        .collect()
}

pub(crate) fn due_at(todo: &super::ShiftTodo, shipped_ms: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(todo.hypothesis.due_ms(shipped_ms)?)
}

fn deployed_ms(legs: &[crate::web::outcomes::project::Leg]) -> Option<i64> {
    let merged: Vec<_> = legs.iter().filter(|leg| leg.merged_ms.is_some()).collect();
    if merged.is_empty() {
        return None;
    }
    merged
        .into_iter()
        .map(|leg| leg.deployed_ms)
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn front_matter_round_trip_and_edit_reach_the_work_api() {
        let text = "+++\nid = \"idea-1\"\nfamily = \"acme\"\nhypothesis = \"Users finish checkout faster.\"\ncheck_after = \"2026-10-15\"\noutcome = \"not_checked\"\noutcome_note = \"\"\n+++\nCount checkout time.\n";
        let mut file = TodoFile::parse(text).unwrap();
        let api = serde_json::to_value(file.to_api(Utc::now())).unwrap();
        assert_eq!(api["hypothesis"], "Users finish checkout faster.");
        assert_eq!(api["check_after"], "2026-10-15");
        assert!(file.dump().contains("check_after = \"2026-10-15\""));
        Hypothesis::edit(
            &mut file,
            &json!({"outcome":"unclear", "outcome_note":"Too few samples.", "check_after":7}),
        )
        .unwrap();
        let reread = TodoFile::parse(&file.dump()).unwrap().to_api(Utc::now());
        assert_eq!(reread.hypothesis.outcome, Outcome::Unclear);
        assert_eq!(reread.hypothesis.check_after, Some(CheckAfter::Days(7)));
        assert_eq!(reread.hypothesis.outcome_note, "Too few samples.");
        Hypothesis::edit(&mut file, &json!({"check_after":null})).unwrap();
        assert_eq!(Hypothesis::from_file(&file).unwrap().check_after, None);
        for value in [
            json!({"outcome":"yes"}),
            json!({"check_after":-1}),
            json!({"check_after":3651}),
            json!({"check_after":"tomorrow"}),
            json!({"hypothesis":"one\ntwo"}),
            json!({"other":"field"}),
        ] {
            assert!(Hypothesis::edit(&mut file, &value).is_err(), "{value}");
        }
    }

    #[test]
    fn outcome_check_is_for_the_owner_with_write_access_or_admin() {
        use super::super::assignment::{Caller, Standing, authorize, may_attempt};
        let mut todo = TodoFile::new("idea-1".into(), "acme".into(), "Checkout".into());
        todo.requested_by = "owner".into();
        let owner = Caller {
            login: "owner",
            person: true,
            admin: false,
        };
        let standing = Standing {
            works_here: true,
            holder_active: true,
        };
        assert!(may_attempt("outcome_check", owner).is_ok());
        assert!(authorize(&todo, "outcome_check", owner, standing).is_ok());
        assert!(
            authorize(
                &todo,
                "outcome_check",
                Caller {
                    login: "other",
                    ..owner
                },
                standing
            )
            .is_err()
        );
        assert!(
            authorize(
                &todo,
                "outcome_check",
                owner,
                Standing {
                    works_here: false,
                    ..standing
                }
            )
            .is_err()
        );
        assert!(
            may_attempt(
                "outcome_check",
                Caller {
                    person: false,
                    ..owner
                }
            )
            .is_err()
        );
        todo.assignee = "holder".into();
        assert!(authorize(&todo, "outcome_check", owner, standing).is_err());
        assert!(
            authorize(
                &todo,
                "outcome_check",
                Caller {
                    login: "holder",
                    ..owner
                },
                standing
            )
            .is_ok()
        );
        assert!(
            authorize(
                &todo,
                "outcome_check",
                Caller {
                    admin: true,
                    ..owner
                },
                standing
            )
            .is_ok()
        );
    }
}
