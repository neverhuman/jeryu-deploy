//! The changes-requested review behind a held pull request, as data: whoever
//! acts on a `pr_changes_requested` row files fix work from this instead of
//! scraping the review text.

use serde::Serialize;

/// Findings a row carries at most; the review itself keeps the rest.
pub(crate) const MAX_FINDINGS: usize = 10;
/// Longest finding title kept, in characters.
const MAX_TITLE_CHARS: usize = 200;

/// One row of the review's findings table.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ReviewFinding {
    /// `critical`, `high`, `medium` or `low`.
    pub severity: String,
    pub title: String,
}

/// The review holding a pull request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ReviewFacts {
    /// The head the review was written against.
    pub head_sha: String,
    pub reviewer: String,
    /// At most [`MAX_FINDINGS`], in the order the review lists them.
    pub findings: Vec<ReviewFinding>,
    /// Findings the review lists beyond those shown.
    #[serde(skip_serializing_if = "is_zero")]
    pub more_findings: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl ReviewFacts {
    pub(crate) fn new(head_sha: String, reviewer: String, body: Option<&str>) -> Self {
        let mut all = body.map(parse_findings).unwrap_or_default();
        let more_findings = all.len().saturating_sub(MAX_FINDINGS);
        all.truncate(MAX_FINDINGS);
        ReviewFacts {
            head_sha,
            reviewer,
            findings: all,
            more_findings,
        }
    }
}

fn severity_word(cell: &str) -> Option<&'static str> {
    let word = cell
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())?
        .to_ascii_lowercase();
    ["critical", "high", "medium", "low"]
        .into_iter()
        .find(|known| *known == word)
}

/// Split a table row into cells on unescaped `|`, unescaping `\|`.
fn cells(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                chars.next();
                cell.push('|');
            }
            '|' => out.push(std::mem::take(&mut cell)),
            _ => cell.push(c),
        }
    }
    out.push(cell);
    out
}

/// The findings table pr-redteam writes: `| <emoji> <severity> | **title** —
/// detail | `where` |`. Rows that do not have a known severity and a bold title
/// are skipped, so prose and the header are never findings.
pub(crate) fn parse_findings(body: &str) -> Vec<ReviewFinding> {
    body.lines()
        .filter(|line| line.trim_start().starts_with('|'))
        .filter_map(|line| {
            let cells = cells(line);
            // A leading and trailing empty cell surround the real ones.
            let severity = severity_word(cells.get(1)?)?;
            let detail = cells.get(2)?;
            let start = detail.find("**")? + 2;
            let len = detail[start..].find("**")?;
            let title: String = detail[start..start + len]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(MAX_TITLE_CHARS)
                .collect();
            (!title.is_empty()).then(|| ReviewFinding {
                severity: severity.to_string(),
                title,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "**Red team: HOLD — changes or further review required.**\n\n\
        Acme billing change needs work.\n\n\
        | | Finding | Where |\n|---|---|---|\n\
        | 🟠 high | **Refund total ignores \\| tax** — detail one | `src/refund.rs:42` |\n\
        | 🟡 medium | **Globex retry is unbounded** — loops | `src/retry.rs` |\n\n\
        <sub>pr-redteam</sub>";

    #[test]
    fn parses_severity_and_title_from_the_table() {
        let found = parse_findings(BODY);
        assert_eq!(
            found,
            vec![
                ReviewFinding {
                    severity: "high".into(),
                    title: "Refund total ignores | tax".into()
                },
                ReviewFinding {
                    severity: "medium".into(),
                    title: "Globex retry is unbounded".into()
                },
            ]
        );
    }

    #[test]
    fn prose_and_no_findings_give_nothing() {
        assert!(parse_findings("No findings.").is_empty());
        assert!(parse_findings("| a | b |\n| high | no bold |").is_empty());
    }

    #[test]
    fn findings_are_capped_and_the_rest_counted() {
        let mut body = String::from("| | Finding | Where |\n|---|---|---|\n");
        for n in 0..13 {
            body.push_str(&format!("| ⚪ low | **Issue {n}** — d | `f` |\n"));
        }
        let facts = ReviewFacts::new("abc".into(), "acme-reviewer".into(), Some(&body));
        assert_eq!(facts.findings.len(), MAX_FINDINGS);
        assert_eq!(facts.more_findings, 3);
        assert_eq!(facts.findings[0].title, "Issue 0");
    }
}
