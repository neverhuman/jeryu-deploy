//! Merged work no release would include: a dependency's default branch moved
//! past what a deploy repo pins. Facts come from [`super::super::pins`].

use chrono::{DateTime, Duration, Utc};

use super::super::pins::{Consumer, Pin};
use super::{Draft, Hosts, Item, Severity, Shell, parse_time};

const UNRELEASED_HREF: &str = "/unreleased";
/// How long auto-pin gets before a missing bump is somebody's problem: its
/// timer fires every 5 minutes and a build takes about 3, so by now it has
/// had several whole turns.
const AUTO_PIN_GRACE_MINUTES: i64 = 20;
const AUTO_PIN_UNIT: &str = "jeryu-auto-pin.service";

/// Whether the dependency's newest commit is recent enough that auto-pin may
/// simply not have finished. An unknown commit time counts as old: asking for
/// a look is the safe side.
fn auto_pin_may_still_be_working(pin: &Pin, now: DateTime<Utc>) -> bool {
    pin.latest_at
        .as_deref()
        .and_then(parse_time)
        .is_some_and(|latest| now - latest < Duration::minutes(AUTO_PIN_GRACE_MINUTES))
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

fn name_of(full_name: &str) -> &str {
    full_name.rsplit('/').next().unwrap_or(full_name)
}

fn commits(count: u64) -> String {
    match count {
        1 => "1 merged commit".to_string(),
        count => format!("{count} merged commits"),
    }
}

/// The newest few subjects, so the reader sees what is waiting without a click.
fn preview(pin: &Pin) -> String {
    let subjects: Vec<&str> = pin
        .unreleased
        .iter()
        .take(3)
        .map(|commit| commit.subject.as_str())
        .collect();
    match subjects.is_empty() {
        true => String::new(),
        false => format!(" Newest: {}.", subjects.join("; ")),
    }
}

fn pin_item(consumer: &Consumer, pin: &Pin, hosts: &Hosts, now: DateTime<Utc>) -> Item {
    let dependency = name_of(&pin.dependency);
    let deploy = name_of(&consumer.repo);
    let id = format!("pin-behind:{}:{}", consumer.repo, pin.dependency);
    let waiting = format!(
        "{dependency} has {} on its default branch that {deploy} does not pin yet, so a \
         release of {deploy} would not include them.{}",
        commits(pin.behind),
        preview(pin)
    );
    let draft = match (pin.kind, &pin.bump_pr) {
        ("tag", _) => Draft {
            id,
            kind: "pin_behind",
            severity: Severity::Watch,
            title: format!(
                "{dependency} has {} since tag {}",
                commits(pin.behind),
                pin.pinned_ref
            ),
            reason: format!(
                "{waiting} {deploy} takes {dependency} by git tag in {}, and nothing cuts tags \
                 or bumps them automatically: releasing this work means cutting the next \
                 {dependency} tag and pointing {deploy} at it.",
                pin.source
            ),
            href: UNRELEASED_HREF.to_string(),
            label: "See what is waiting for a tag",
            command: None,
        },
        (_, Some(bump)) => Draft {
            id,
            kind: "pin_behind",
            severity: Severity::Watch,
            title: format!(
                "{deploy} #{} bumps the {dependency} pin ({})",
                bump.number,
                commits(pin.behind)
            ),
            reason: format!(
                "{waiting} The bump is already open as a pull request; once it is reviewed \
                 and lands, the next staged release includes this work."
            ),
            href: bump.url.clone(),
            label: "Follow the pin bump pull request",
            command: None,
        },
        (_, None) if auto_pin_may_still_be_working(pin, now) => Draft {
            id,
            kind: "pin_behind",
            severity: Severity::Watch,
            title: format!(
                "{} of {dependency} are being pinned by auto-pin",
                commits(pin.behind)
            ),
            reason: format!(
                "{waiting} The auto-pin timer opens the bump pull request within a few minutes \
                 of {dependency} going green, so there is nothing to do yet."
            ),
            href: UNRELEASED_HREF.to_string(),
            label: "See what is waiting",
            command: None,
        },
        (_, None) => Draft {
            id,
            kind: "pin_behind",
            severity: Severity::Action,
            title: format!(
                "{} of {dependency} are not in {deploy}'s pin",
                commits(pin.behind)
            ),
            reason: format!(
                "The auto-pin timer should have opened this bump and has not. Starting it \
                 builds the web bundle, changes the two lock fields (`commit` and \
                 `web_dist_sha256` for {dependency} in {}) and opens the pull request; its log \
                 is `journalctl --user -u {AUTO_PIN_UNIT} -n 30`. {waiting}",
                pin.source
            ),
            href: UNRELEASED_HREF.to_string(),
            label: "Run auto-pin now",
            command: Some(Shell {
                line: format!("systemctl --user start {AUTO_PIN_UNIT}"),
                run_in: Hosts::anywhere(&hosts.release),
            }),
        },
    };
    let mut item = draft.build();
    item.since = pin.latest_at.clone();
    item.family = consumer.family.clone();
    item.repo = Some(consumer.repo.clone());
    item.pr = pin.bump_pr.as_ref().map(|bump| bump.number);
    item.sha = pin.latest_sha.as_deref().map(short);
    item
}

/// One item per pin whose dependency has green, merged work it does not reach.
pub(crate) fn pin_items(consumers: &[Consumer], hosts: &Hosts, now: DateTime<Utc>) -> Vec<Item> {
    consumers
        .iter()
        .flat_map(|consumer| {
            consumer
                .pins
                .iter()
                .filter(|pin| pin.state == "behind")
                .map(move |pin| pin_item(consumer, pin, hosts, now))
        })
        .collect()
}
