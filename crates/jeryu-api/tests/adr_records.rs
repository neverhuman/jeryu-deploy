//! The architecture decision records in `docs/adr/` are a durable record, so
//! their shape is checked rather than trusted: numbering, the four header
//! lines, the section order, and supersession that points both ways.
//!
//! The convention itself is written down in `docs/adr/README.md`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const STATUSES: &[&str] = &["Proposed", "Accepted", "Rejected", "Superseded"];
const SECTIONS: &[&str] = &["## Context", "## Decision", "## Consequences"];

struct Record {
    number: u32,
    file: String,
    body: String,
}

impl Record {
    /// The value of a `Name: value` header line, which every record carries.
    fn header(&self, name: &str) -> String {
        let prefix = format!("{name}: ");
        self.body
            .lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .unwrap_or_else(|| panic!("{}: missing `{name}:` header line", self.file))
            .trim()
            .to_string()
    }

    /// Record numbers named by a `Supersedes:`/`Superseded-by:` line.
    fn linked(&self, name: &str) -> Vec<u32> {
        let value = self.header(name);
        if value == "none" {
            return Vec::new();
        }
        value
            .split(',')
            .map(|entry| {
                let entry = entry.trim();
                assert_eq!(
                    entry.len(),
                    4,
                    "{}: `{name}: {value}` must name four-digit record numbers",
                    self.file
                );
                entry.parse().unwrap_or_else(|_| {
                    panic!("{}: `{name}: {value}` is not a record number", self.file)
                })
            })
            .collect()
    }
}

fn adr_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/adr")
}

fn records() -> BTreeMap<u32, Record> {
    let directory = adr_directory();
    let mut records = BTreeMap::new();
    for entry in fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
    {
        let path = entry.expect("read docs/adr entry").path();
        let file = path
            .file_name()
            .expect("docs/adr entry has a name")
            .to_string_lossy()
            .to_string();
        if file == "README.md" {
            continue;
        }
        assert!(
            file.ends_with(".md"),
            "docs/adr holds only markdown records, found {file}"
        );
        let (digits, rest) = file.split_at(4);
        let number: u32 = digits
            .parse()
            .unwrap_or_else(|_| panic!("{file}: name must start with a four-digit number"));
        assert!(
            rest.starts_with('-') && rest.len() > 4,
            "{file}: name must be NNNN-kebab-title.md"
        );
        let body = fs::read_to_string(&path).unwrap_or_else(|error| panic!("read {file}: {error}"));
        let previous = records.insert(number, Record { number, file, body });
        assert!(previous.is_none(), "two records numbered {number}");
    }
    assert!(!records.is_empty(), "docs/adr holds no records");
    records
}

#[test]
fn every_record_carries_the_standard_shape() {
    for record in records().values() {
        let heading = record
            .body
            .lines()
            .next()
            .unwrap_or_else(|| panic!("{}: empty record", record.file));
        let expected_heading = format!("# {:04}.", record.number);
        assert!(
            heading.starts_with(&expected_heading),
            "{}: heading must open with `{expected_heading}`",
            record.file
        );

        let status = record.header("Status");
        assert!(
            STATUSES.contains(&status.as_str()),
            "{}: status {status} is not one of {STATUSES:?}",
            record.file
        );
        let date = record.header("Date");
        assert!(
            date.len() == 10 && date.split('-').count() == 3,
            "{}: date {date} is not YYYY-MM-DD",
            record.file
        );
        // Both lines are always written out, so an absent one is never
        // mistaken for "nothing was superseded".
        record.header("Supersedes");
        record.header("Superseded-by");

        let mut offset = 0;
        for section in SECTIONS {
            let found = record.body[offset..]
                .find(&format!("\n{section}\n"))
                .unwrap_or_else(|| panic!("{}: missing `{section}` in order", record.file));
            offset += found + section.len();
        }
    }
}

#[test]
fn supersession_points_both_ways() {
    let records = records();
    for record in records.values() {
        for superseded in record.linked("Supersedes") {
            let other = records
                .get(&superseded)
                .unwrap_or_else(|| panic!("{}: supersedes missing {superseded:04}", record.file));
            assert_eq!(
                other.header("Status"),
                "Superseded",
                "{}: superseded by {}, so its status must be Superseded",
                other.file,
                record.file
            );
            assert!(
                other.linked("Superseded-by").contains(&record.number),
                "{}: must name {:04} in its `Superseded-by:` line",
                other.file,
                record.number
            );
        }
        for successor in record.linked("Superseded-by") {
            let other = records
                .get(&successor)
                .unwrap_or_else(|| panic!("{}: superseded by missing {successor:04}", record.file));
            assert!(
                other.linked("Supersedes").contains(&record.number),
                "{}: must name {:04} in its `Supersedes:` line",
                other.file,
                record.number
            );
        }
    }
}

#[test]
fn the_seeded_decisions_are_recorded() {
    let records = records();
    for (number, expected) in [
        (1u32, "forge-is-origin-github-is-a-downstream-mirror"),
        (2, "html-url-is-jeryu-shaped"),
        (3, "gating-runs-on-a-dedicated-gate-host"),
    ] {
        let record = records
            .get(&number)
            .unwrap_or_else(|| panic!("record {number:04} is missing"));
        assert_eq!(record.file, format!("{number:04}-{expected}.md"));
        assert_eq!(record.header("Status"), "Accepted");
    }
}
