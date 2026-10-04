//! The family release board: a lane shown red, a source the collector could
//! not read, and a board nobody has refreshed.
//!
//! `/releases` draws all three (a red stage, the problem list, the "stale"
//! pill) and nothing else notices them, so a board left red over a weekend
//! told nobody. One item per red lane, because each lane is its own
//! deliverable and its own next step; one item for the collector's unread
//! sources of a family, because they share one host and one adapter; one
//! `watch` for a board that stopped arriving, which is about the collector
//! rather than about any lane.

use chrono::{DateTime, Utc};

use super::{Draft, Item, Severity, parse_time};

/// A board observed longer ago than this is no longer the picture of now. The
/// collector posts every five minutes, and `/releases` flags the same age.
pub(super) const BOARD_STALE_MINUTES: i64 = 15;

/// One stored board, as the rule needs it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BoardFacts {
    /// Canonical family key, as the board is stored under.
    pub family: String,
    /// `observed_at` as the snapshot spells it; empty when it carries none.
    pub observed_at: String,
    pub lanes: Vec<BoardLane>,
    /// `<source>: <message>` per source the collector could not read.
    pub problems: Vec<String>,
}

/// One lane of a board, with the stages it draws red.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BoardLane {
    pub id: String,
    pub name: String,
    /// `<stage name>: <status>` per stage in state `bad`, left to right.
    pub red_stages: Vec<String>,
}

/// The lane's place on the board: `/releases` scrolls to `#lane-<id>` and
/// rings the lane, so the reader lands on the red stage and not on the top of
/// a long board.
fn lane_href(family: &str, lane: &str) -> String {
    format!("/releases/family/{family}#lane-{lane}")
}

fn board_href(family: &str) -> String {
    format!("/releases/family/{family}")
}

/// What a sentence calls a count of stages or sources.
fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Everything the release boards show that waits on a person.
pub(crate) fn board_items(boards: &[BoardFacts], now: DateTime<Utc>) -> Vec<Item> {
    let mut items = Vec::new();
    for board in boards {
        for lane in &board.lanes {
            if lane.red_stages.is_empty() {
                continue;
            }
            let mut item = Draft {
                id: format!("release-board-problem-{}-{}", board.family, lane.id),
                kind: "release_board_problem",
                severity: Severity::Action,
                title: format!(
                    "The {} lane is red on the {} release board",
                    lane.name, board.family
                ),
                reason: format!(
                    "{} of the lane {} red: {}. The board says what the family runs, so the \
                     stage stays red until the release is deployed, rolled back, or the lane's \
                     adapter is told what it now reads.",
                    plural(lane.red_stages.len(), "stage", "stages"),
                    if lane.red_stages.len() == 1 {
                        "is"
                    } else {
                        "are"
                    },
                    lane.red_stages.join("; "),
                ),
                href: lane_href(&board.family, &lane.id),
                label: "Read the red stage on the board and deploy, roll back or fix its adapter",
                api: None,
                command: None,
            }
            .build();
            item.family = Some(board.family.clone());
            items.push(item);
        }
        if !board.problems.is_empty() {
            let mut item = Draft {
                id: format!("release-board-problem-{}-sources", board.family),
                kind: "release_board_problem",
                severity: Severity::Action,
                title: format!(
                    "The {} board collector could not read {}",
                    board.family,
                    plural(board.problems.len(), "source", "sources"),
                ),
                reason: format!(
                    "{}. Every stage behind those sources is as old as the last run that could \
                     read them, so the board is not wrong about them, only quiet; the fix is in \
                     the family's adapter or in what it reaches (a host, a registry, a \
                     manifest).",
                    board.problems.join("; "),
                ),
                href: board_href(&board.family),
                label: "Fix what the family's board adapter could not read",
                api: None,
                command: None,
            }
            .build();
            item.family = Some(board.family.clone());
            items.push(item);
        }
        let observed = parse_time(&board.observed_at);
        let stale = observed
            .is_none_or(|at| now.signed_duration_since(at).num_minutes() >= BOARD_STALE_MINUTES);
        if !stale {
            continue;
        }
        let mut item = Draft {
            id: format!("release-board-stale-{}", board.family),
            kind: "release_board_problem",
            severity: Severity::Watch,
            title: format!("The {} release board has stopped arriving", board.family),
            reason: match observed {
                Some(at) => format!(
                    "The newest snapshot was observed at {}, more than {BOARD_STALE_MINUTES} \
                     minutes ago; the collector posts one every five minutes. What the page shows \
                     is that old, so a release made since then is not on it.",
                    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                ),
                None => "The stored snapshot carries no readable `observed_at`, so how old the \
                         board is cannot be told."
                    .to_string(),
            },
            href: board_href(&board.family),
            label: "Check the family's board collector timer on its host",
            api: None,
            command: None,
        }
        .build();
        item.since = observed.map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        item.family = Some(board.family.clone());
        items.push(item);
    }
    items
}
