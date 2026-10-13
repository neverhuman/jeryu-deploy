use std::collections::{BTreeMap, BTreeSet};

use toml_edit::Value;

use super::todo_file::TodoFile;
use super::types::{TodoLanding, TodoLandingState};

const RESCUE_PREFIX: &str = "todoq-rescue/";

pub(super) fn from_file(todo: &TodoFile) -> Vec<TodoLanding> {
    let mut entries = base_entries(todo);
    if !apply_structured(todo, &mut entries) {
        parse_notes(todo, &mut entries);
    }
    entries.into_values().collect()
}

fn base_entries(todo: &TodoFile) -> BTreeMap<String, TodoLanding> {
    let mut repos: BTreeSet<String> = todo.repos.iter().cloned().collect();
    repos.extend(todo.commits.iter().map(|(repo, _)| repo.clone()));
    let mut entries = BTreeMap::new();
    for repo in repos {
        let mut entry = TodoLanding {
            repo: repo.clone(),
            state: TodoLandingState::Pending,
            target: (!todo.shift.is_empty()).then(|| todo.shift.clone()),
            sha: todo
                .commits
                .iter()
                .find(|(name, _)| name == &repo)
                .map(|(_, sha)| sha.clone()),
            main_sha: None,
            rescue_branch: None,
            rescue_sha: None,
            rescue_ahead: None,
            failing: Vec::new(),
            check: None,
        };
        if todo.merged {
            entry.state = TodoLandingState::Landed;
        }
        entries.insert(repo, entry);
    }
    entries
}

fn apply_structured(todo: &TodoFile, entries: &mut BTreeMap<String, TodoLanding>) -> bool {
    let Some((_, value)) = todo.extra.iter().find(|(key, _)| key == "landing") else {
        return false;
    };
    let Some(table) = value.as_inline_table() else {
        return false;
    };
    for (repo, value) in table.iter() {
        let Some(fields) = value.as_inline_table() else {
            continue;
        };
        let entry = entries
            .entry(repo.to_string())
            .or_insert_with(|| empty(repo.to_string()));
        apply_fields(fields, entry);
    }
    true
}

fn apply_fields(fields: &toml_edit::InlineTable, entry: &mut TodoLanding) {
    if let Some(state) = text_field(fields, "state").and_then(parse_state) {
        entry.state = state;
    }
    entry.target = text_field(fields, "target").or_else(|| entry.target.clone());
    entry.sha = text_field(fields, "sha").or_else(|| entry.sha.clone());
    entry.main_sha = text_field(fields, "main_sha");
    entry.rescue_branch = text_field(fields, "rescue_branch");
    entry.rescue_sha = text_field(fields, "rescue_sha");
    entry.rescue_ahead = fields
        .get("rescue_ahead")
        .and_then(Value::as_integer)
        .or(entry.rescue_ahead);
    entry.check = text_field(fields, "check");
    if let Some(failing) = fields.get("failing").and_then(Value::as_array) {
        entry.failing = failing
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .take(10)
            .collect();
    }
}

fn parse_notes(todo: &TodoFile, entries: &mut BTreeMap<String, TodoLanding>) {
    let newest_attempt_note = todo
        .attempts_list()
        .last()
        .map(|attempt| attempt.note.clone());
    for note in [Some(todo.note.as_str()), newest_attempt_note.as_deref()]
        .into_iter()
        .flatten()
        .filter(|note| !note.trim().is_empty())
    {
        parse_note(note, entries);
    }
}

fn parse_note(note: &str, entries: &mut BTreeMap<String, TodoLanding>) {
    parse_already_landed(note, entries);
    let Some(rest) = note.strip_prefix("could not land ") else {
        return;
    };
    let Some((repo, rest)) = rest.split_once(" on ") else {
        return;
    };
    let repo = repo.trim();
    let Some((target, rest)) = rest.split_once(": ") else {
        return;
    };
    let failure = rest.split(';').next().unwrap_or(rest).trim();
    let entry = entries
        .entry(repo.to_string())
        .or_insert_with(|| empty(repo.to_string()));
    entry.target = Some(target.trim().to_string());
    apply_failure(failure, entry);
    if let Some((branch, rescue_repo)) = parse_rescue(note) {
        let rescued = entries
            .entry(rescue_repo.clone())
            .or_insert_with(|| empty(rescue_repo.clone()));
        rescued.rescue_branch = Some(branch);
        if rescued.state == TodoLandingState::Pending {
            rescued.state = TodoLandingState::Kept;
        }
    }
}

fn apply_failure(failure: &str, entry: &mut TodoLanding) {
    let lower = failure.to_lowercase();
    if let Some(digest) = failure.strip_prefix("the gate failed after rebasing onto the target:") {
        entry.state = TodoLandingState::GateFailed;
        entry.failing = split_failing(digest);
    } else if lower.contains("conflict") {
        entry.state = TodoLandingState::Conflict;
    } else if lower.contains("push race") || lower.contains("lost") && lower.contains("race") {
        entry.state = TodoLandingState::PushRace;
    } else {
        entry.state = TodoLandingState::Kept;
    }
}

fn parse_already_landed(note: &str, entries: &mut BTreeMap<String, TodoLanding>) {
    let Some((_, rest)) = note.split_once("already landed:") else {
        return;
    };
    let dict = rest.split(')').next().unwrap_or(rest);
    let Some(start) = dict.find('{') else {
        return;
    };
    let Some(end) = dict[start..].find('}') else {
        return;
    };
    let inner = &dict[start + 1..start + end];
    for pair in inner.split(',') {
        let Some((repo, sha)) = pair.split_once(':') else {
            continue;
        };
        let repo = unquote(repo.trim());
        let sha = unquote(sha.trim());
        if repo.is_empty() || sha.is_empty() {
            continue;
        }
        let entry = entries
            .entry(repo.to_string())
            .or_insert_with(|| empty(repo.to_string()));
        entry.state = TodoLandingState::Landed;
        entry.sha = Some(sha.to_string());
    }
}

fn parse_rescue(note: &str) -> Option<(String, String)> {
    let (_, rest) = note.split_once("finished work kept on ")?;
    let branch = rest
        .split_whitespace()
        .next()?
        .trim_matches(|c: char| c == ';' || c == ',' || c == '.');
    if !branch.starts_with(RESCUE_PREFIX) {
        return None;
    }
    let (_, repo_rest) = rest.split_once(" in ")?;
    let repo = repo_rest
        .split(|c: char| c == ';' || c == ',' || c == ')' || c.is_whitespace())
        .next()
        .unwrap_or("")
        .trim();
    (!repo.is_empty()).then(|| (branch.to_string(), repo.to_string()))
}

fn split_failing(text: &str) -> Vec<String> {
    text.split([',', ';', '\n'])
        .map(|part| part.trim().trim_matches('.'))
        .filter(|part| !part.is_empty())
        .take(10)
        .map(str::to_string)
        .collect()
}

fn text_field(fields: &toml_edit::InlineTable, key: &str) -> Option<String> {
    fields
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_state(state: String) -> Option<TodoLandingState> {
    match state.as_str() {
        "landed" => Some(TodoLandingState::Landed),
        "gate_failed" => Some(TodoLandingState::GateFailed),
        "conflict" => Some(TodoLandingState::Conflict),
        "push_race" => Some(TodoLandingState::PushRace),
        "kept" => Some(TodoLandingState::Kept),
        "pending" => Some(TodoLandingState::Pending),
        _ => None,
    }
}

fn empty(repo: String) -> TodoLanding {
    TodoLanding {
        repo,
        state: TodoLandingState::Pending,
        target: None,
        sha: None,
        main_sha: None,
        rescue_branch: None,
        rescue_sha: None,
        rescue_ahead: None,
        failing: Vec::new(),
        check: None,
    }
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches('"').trim_matches('\'').trim()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use toml_edit::Value;

    use super::super::todo_file::TodoFile;
    use super::super::types::TodoLandingState;

    fn todo(note: &str) -> TodoFile {
        let mut todo = TodoFile::new(
            "20261013-120000-abcdef".to_string(),
            "acme".to_string(),
            "Ship widgets".to_string(),
        );
        todo.repos = vec!["acme-api".to_string(), "acme-web".to_string()];
        todo.shift = "nightshift/2026-10-12".to_string();
        todo.note = note.to_string();
        todo
    }

    #[test]
    fn parses_gate_failed_rescue_partial_and_already_landed() {
        let api = todo(
            "could not land acme-web on nightshift/2026-10-12: the gate failed after rebasing onto the target: cargo test, clippy; finished work kept on todoq-rescue/20261013-120000-abcdef in acme-web; already landed: {'acme-api': '11112222'} (partial change set cs-1)",
        )
        .to_api(Utc::now());
        let api_repo = api
            .landing
            .iter()
            .find(|entry| entry.repo == "acme-api")
            .unwrap();
        assert_eq!(api_repo.state, TodoLandingState::Landed);
        assert_eq!(api_repo.sha.as_deref(), Some("11112222"));
        let web = api
            .landing
            .iter()
            .find(|entry| entry.repo == "acme-web")
            .unwrap();
        assert_eq!(web.state, TodoLandingState::GateFailed);
        assert_eq!(web.target.as_deref(), Some("nightshift/2026-10-12"));
        assert_eq!(
            web.rescue_branch.as_deref(),
            Some("todoq-rescue/20261013-120000-abcdef")
        );
        assert_eq!(web.failing, ["cargo test", "clippy"]);
    }

    #[test]
    fn parses_conflict_and_push_race_without_rescue() {
        let conflict =
            todo("could not land acme-api on main: conflict while rebasing").to_api(Utc::now());
        assert_eq!(conflict.landing[0].state, TodoLandingState::Conflict);
        assert!(conflict.landing[0].rescue_branch.is_none());

        let race = todo("could not land acme-api on main: lost push race").to_api(Utc::now());
        assert_eq!(race.landing[0].state, TodoLandingState::PushRace);
        assert!(race.landing[0].rescue_branch.is_none());
    }

    #[test]
    fn parses_last_attempt_note_when_todo_note_has_no_rescue() {
        let text = r#"+++
id = "20261013-120000-abcdef"
family = "acme"
title = "Ship widgets"
repos = ["acme-api"]
status = "blocked"
note = ""
worked_by = [{ "by" = "operator@node-a/w1", "host" = "node-a", "slot" = "w1", "model" = "model-a", "session" = "s-1", "started" = "2026-10-13T12:00:00Z", "ended" = "2026-10-13T12:10:00Z", "outcome" = "blocked", "note" = "could not land acme-api on main: the gate failed after rebasing onto the target: unit", "shift" = "" }]
+++
body
"#;
        let api = TodoFile::parse(text).unwrap().to_api(Utc::now());
        assert_eq!(api.landing[0].state, TodoLandingState::GateFailed);
        assert_eq!(api.landing[0].failing, ["unit"]);
        assert!(api.landing[0].rescue_branch.is_none());
    }

    #[test]
    fn structured_landing_front_matter_wins() {
        let mut todo = todo("could not land acme-api on main: conflict");
        let value: Value = r#"{ "acme-api" = { state = "kept", target = "main", rescue_branch = "todoq-rescue/20261013-120000-abcdef", rescue_ahead = 2, failing = ["unit"] } }"#
            .parse()
            .unwrap();
        todo.extra.push(("landing".to_string(), value));
        let api = todo.to_api(Utc::now());
        let entry = api
            .landing
            .iter()
            .find(|entry| entry.repo == "acme-api")
            .unwrap();
        assert_eq!(entry.state, TodoLandingState::Kept);
        assert_eq!(entry.rescue_ahead, Some(2));
        assert_eq!(entry.failing, ["unit"]);
    }
}
