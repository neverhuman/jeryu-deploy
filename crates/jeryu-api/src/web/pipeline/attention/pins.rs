//! Merged work no release would include: a dependency's default branch moved
//! past what a deploy repo pins. Facts come from [`super::super::pins`].

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};

use super::super::pins::{Consumer, Pin};
use super::{Draft, Hosts, Item, Severity, Shell, parse_time, releases_href};
use crate::web::pipeline::Event;

/// How long auto-pin gets before a missing bump is somebody's problem: its
/// timer fires every 5 minutes and a build takes about 3, so by now it has
/// had several whole turns.
const AUTO_PIN_GRACE_MINUTES: i64 = 20;
/// A web pin this far behind a green main is no longer a release-page
/// curiosity; production is missing shipped UI work until the bump lands.
const WEB_PIN_ACTION_HOURS: i64 = 12;
/// A release pin bump that sits open this long without merging is holding the
/// web UI even without a review verdict.
const WEB_BUMP_OPEN_HOURS: i64 = 6;
const AUTO_PIN_UNIT: &str = "jeryu-auto-pin.service";
/// Where auto-pin counts failures per dependency commit; a file there at the
/// limit is its give-up marker for that commit.
const AUTO_PIN_FAILURES: &str = "~/.local/state/jeryu-auto-pin/failures";

/// The newest give-up (`pin.bump_failed` with `needs_human`) for the
/// dependency's current head. `gave_up` is newest first; a give-up for an
/// older head says nothing about this one.
fn gave_up_on<'a>(pin: &Pin, events: &'a [Event]) -> Option<&'a Event> {
    let head = pin.latest_sha.as_deref()?;
    events.iter().find(|event| {
        event.kind == "pin.bump_failed" && event.needs_human && event.sha.as_deref() == Some(head)
    })
}

fn latest_pin_event_for<'a>(pin: &Pin, events: &'a [Event]) -> Option<&'a Event> {
    let head = pin.latest_sha.as_deref()?;
    events.iter().find(|event| {
        matches!(event.kind.as_str(), "pin.bump_opened" | "pin.bump_failed")
            && event.sha.as_deref() == Some(head)
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

fn hours_since(at: Option<&str>, now: DateTime<Utc>) -> Option<i64> {
    at.and_then(parse_time).map(|then| (now - then).num_hours())
}

fn hours_text(hours: i64) -> String {
    match hours {
        1 => "1 hour".to_string(),
        hours => format!("{hours} hours"),
    }
}

fn is_web_pin(pin: &Pin) -> bool {
    name_of(&pin.dependency) == "jeryu-web" && pin.kind == "commit"
}

fn is_web_bump(pin: &Pin) -> bool {
    pin.bump_pr.as_ref().is_some_and(|bump| {
        bump.title.starts_with("release: pin jeryu-web ")
            && bump.head_ref.starts_with("auto/pin-web-")
    })
}

fn auto_pin_state(pin: &Pin, events: &[Event]) -> String {
    match latest_pin_event_for(pin, events) {
        Some(event) if event.kind == "pin.bump_opened" => match event.pr {
            Some(number) => format!("auto-pin last emitted pin.bump_opened for PR #{number}"),
            None => "auto-pin last emitted pin.bump_opened without a PR number".to_string(),
        },
        Some(event) if event.kind == "pin.bump_failed" => {
            let reason =
                failure_text(event).unwrap_or_else(|| "auto-pin recorded no reason".to_string());
            format!("auto-pin last emitted pin.bump_failed: {reason}")
        }
        Some(event) => format!("auto-pin last emitted {}", event.kind),
        None => "auto-pin has emitted no pin.bump_opened or pin.bump_failed event for this head"
            .to_string(),
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
    pin_events: &[Event],
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
    let gave_up = gave_up_on(pin, pin_events);
    let web_age_hours = hours_since(pin.latest_at.as_deref(), now);
    let web_action_age = is_web_pin(pin)
        && pin.latest_green == Some(true)
        && web_age_hours.is_some_and(|hours| hours >= WEB_PIN_ACTION_HOURS);
    let web_age_note = || {
        web_action_age.then(|| {
            let age = hours_text(web_age_hours.unwrap_or(0));
            format!(" The shipped web UI pin has been behind green {dependency} main for {age}.")
        })
    };
    // The button that asks the release host to run auto-pin: only while a
    // poller is live and no auto-pin ask is already open.
    let asked = hosts.release_queue.open_auto_pin(&consumer.repo);
    let pin_call = |head: Option<&str>| {
        (hosts.release_queue.polls(&consumer.repo) && asked.is_none())
            .then(|| super::auto_pin_call(&consumer.repo, head))
    };
    let asked_note = asked.map_or_else(String::new, |ask| format!(" {}", ask.progress()));
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
        (_, Some(bump)) if is_web_bump(pin) && bump.changes_requested => {
            let head = pin.latest_sha.as_deref().map(short).unwrap_or_default();
            let review = bump
                .blocking_review
                .clone()
                .unwrap_or_else(|| "a reviewer requested changes".to_string());
            Draft {
                id,
                kind: "pin_behind",
                severity: Severity::Action,
                title: format!(
                    "The web UI is not shipping: {deploy}#{} (pins {dependency} {head}, {} behind main) is held - {review}",
                    bump.number, pin.behind
                ),
                reason: format!(
                    "{waiting} The auto-pin bump is release: pin jeryu-web on {}, and the \
                     current head has changes requested: {review}. This replaces the generic \
                     pull-request review row because the blocked review is holding the shipped \
                     web UI pin.{}",
                    bump.head_ref,
                    web_age_note().unwrap_or_default()
                ),
                href: bump.url.clone(),
                label: "Read the review and push a fix, or dismiss it",
                api: None,
                command: None,
            }
        }
        (_, Some(bump))
            if is_web_bump(pin)
                && hours_since(Some(&bump.created_at), now)
                    .is_some_and(|hours| hours >= WEB_BUMP_OPEN_HOURS) =>
        {
            let head = pin.latest_sha.as_deref().map(short).unwrap_or_default();
            let open_for = hours_text(hours_since(Some(&bump.created_at), now).unwrap_or(0));
            Draft {
                id,
                kind: "pin_behind",
                severity: Severity::Action,
                title: format!(
                    "The web UI is not shipping: {deploy}#{} (pins {dependency} {head}, {} behind main) has been open for {open_for}",
                    bump.number, pin.behind
                ),
                reason: format!(
                    "{waiting} The auto-pin bump is release: pin jeryu-web on {}, but it has \
                     been open for {open_for} without merging. That keeps the shipped web UI pin \
                     behind green main.{}",
                    bump.head_ref,
                    web_age_note().unwrap_or_default()
                ),
                href: bump.url.clone(),
                label: "Review and merge the web pin bump",
                api: None,
                command: None,
            }
        }
        (_, Some(bump)) if web_action_age => {
            let age = hours_text(web_age_hours.unwrap_or(0));
            Draft {
                id,
                kind: "pin_behind",
                severity: Severity::Action,
                title: format!(
                    "{dependency} has been green and ahead of the shipped pin for {age}"
                ),
                reason: format!(
                    "{waiting} The shipped web UI pin has been behind green {dependency} main \
                     for {age}. The open bump meant to move it is {deploy}#{}.",
                    bump.number
                ),
                href: bump.url.clone(),
                label: "Follow the web pin bump pull request",
                api: None,
                command: None,
            }
        }
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
                     `journalctl --user -u {AUTO_PIN_UNIT} -n 30`. {waiting}{asked_note}"
                ),
                href: releases_href(&consumer.repo),
                label: "Clear the give-up and retry auto-pin",
                api: pin_call(Some(head)),
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
                "{}. The auto-pin timer should have opened this bump and has not. Starting it \
                 builds the web bundle, changes the two lock fields (`commit` and \
                 `web_dist_sha256` for {dependency} in {}) and opens the pull request; its log \
                 is `journalctl --user -u {AUTO_PIN_UNIT} -n 30`. {waiting}{asked_note}",
                auto_pin_state(pin, pin_events),
                pin.source
            ),
            href: releases_href(&consumer.repo),
            label: "Run auto-pin now",
            api: pin_call(None),
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
/// `pin_events` are auto-pin's `pin.bump_opened` and `pin.bump_failed` events,
/// newest first.
pub(crate) fn pin_items(
    consumers: &[Consumer],
    pin_events: &[Event],
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
                .map(move |pin| pin_item(consumer, pin, pin_events, hosts, now))
        })
        .collect()
}

pub(crate) fn web_pin_bump_prs(items: &[Item]) -> BTreeSet<(Option<String>, Option<u64>)> {
    items
        .iter()
        .filter(|item| {
            item.kind == "pin_behind"
                && item.severity == Severity::Action
                && item.title.starts_with("The web UI is not shipping:")
        })
        .map(|item| (item.repo.clone(), item.pr))
        .collect()
}
