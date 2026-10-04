//! Merged work no release would include: a dependency's default branch moved
//! past what a deploy repo pins. Facts come from [`super::super::pins`].

use chrono::{DateTime, Duration, Utc};

use super::super::pins::{Consumer, Pin};
use super::{Draft, Hosts, Item, Severity, Shell, parse_time, releases_href};
use crate::web::pipeline::Event;

/// How long auto-pin gets before a missing bump is somebody's problem: its
/// timer fires every 5 minutes and a build takes about 3, so by now it has
/// had several whole turns.
const AUTO_PIN_GRACE_MINUTES: i64 = 20;
const AUTO_PIN_UNIT: &str = "jeryu-auto-pin.service";
/// Where auto-pin counts failures per dependency commit; a file there at the
/// limit is its give-up marker for that commit.
const AUTO_PIN_FAILURES: &str = "~/.local/state/jeryu-auto-pin/failures";

/// The newest give-up (`pin.bump_failed` with `needs_human`) for the
/// dependency's current head. `gave_up` is newest first; a give-up for an
/// older head says nothing about this one.
fn gave_up_on<'a>(pin: &Pin, gave_up: &'a [Event]) -> Option<&'a Event> {
    let head = pin.latest_sha.as_deref()?;
    gave_up.iter().find(|event| {
        event.kind == "pin.bump_failed" && event.needs_human && event.sha.as_deref() == Some(head)
    })
}

/// What went wrong, as one trimmed line: the event's reason, else a string
/// `reason` in its detail, else the last non-blank line of its log tail.
fn failure_text(event: &Event) -> Option<String> {
    let one_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let detail = event
        .detail
        .as_ref()
        .and_then(|detail| detail.get("reason"))
        .and_then(|reason| reason.as_str());
    let tail = event
        .log_tail
        .as_deref()
        .and_then(|tail| tail.lines().rev().find(|line| !line.trim().is_empty()));
    [event.reason.as_deref(), detail, tail]
        .into_iter()
        .flatten()
        .map(one_line)
        .find(|text| !text.is_empty())
}

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

fn pin_item(
    consumer: &Consumer,
    pin: &Pin,
    gave_up: &[Event],
    hosts: &Hosts,
    now: DateTime<Utc>,
) -> Item {
    let dependency = name_of(&pin.dependency);
    let deploy = name_of(&consumer.repo);
    let id = format!("pin-behind:{}:{}", consumer.repo, pin.dependency);
    let waiting = format!(
        "{dependency} has {} on its default branch that {deploy} does not pin yet, so a \
         release of {deploy} would not include them.{}",
        commits(pin.behind),
        preview(pin)
    );
    let gave_up = gave_up_on(pin, gave_up);
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
            href: releases_href(&consumer.repo),
            label: "See what is waiting for a tag",
            api: None,
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
            api: None,
            command: None,
        },
        (_, None) if let (Some(event), Some(head)) = (gave_up, pin.latest_sha.as_deref()) => {
            let failure =
                failure_text(event).unwrap_or_else(|| "auto-pin recorded no reason".to_string());
            let head7 = short(head);
            Draft {
                id,
                kind: "pin_behind",
                severity: Severity::Action,
                title: format!("auto-pin gave up on {dependency} {head7}"),
                reason: format!(
                    "{failure}. auto-pin stopped retrying {dependency} {head7}, so no bump pull \
                     request comes until its give-up marker is cleared; its log is \
                     `journalctl --user -u {AUTO_PIN_UNIT} -n 30`. {waiting}"
                ),
                href: releases_href(&consumer.repo),
                label: "Clear the give-up and retry auto-pin",
                api: None,
                command: Some(Shell {
                    line: format!(
                        "rm -f {AUTO_PIN_FAILURES}/{head} && systemctl --user start {AUTO_PIN_UNIT}"
                    ),
                    run_in: Hosts::anywhere(&hosts.release),
                }),
            }
        }
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
            href: releases_href(&consumer.repo),
            label: "See what is waiting",
            api: None,
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
            href: releases_href(&consumer.repo),
            label: "Run auto-pin now",
            api: None,
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
/// `gave_up` is auto-pin's `pin.bump_failed` events with `needs_human`,
/// newest first.
pub(crate) fn pin_items(
    consumers: &[Consumer],
    gave_up: &[Event],
    hosts: &Hosts,
    now: DateTime<Utc>,
) -> Vec<Item> {
    consumers
        .iter()
        .flat_map(|consumer| {
            consumer
                .pins
                .iter()
                .filter(|pin| pin.state == "behind")
                .map(move |pin| pin_item(consumer, pin, gave_up, hosts, now))
        })
        .collect()
}
