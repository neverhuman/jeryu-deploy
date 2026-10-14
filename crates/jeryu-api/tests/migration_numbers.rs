//! `db/migrations/` numbers are a sequence: one `.sql` file per number,
//! 0001 upwards with no gap, and any `.meta.toml` beside the `.sql` it
//! describes. Two branches cut from the same base can each take "the next"
//! number; this check fails the gate on the combined tree when that happens.
//!
//! The convention is written down in `db/migrations/README.md`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Default)]
struct Number {
    sql: Vec<String>,
    meta: Vec<String>,
}

fn numbers() -> BTreeMap<u32, Number> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../db/migrations");
    let mut numbers: BTreeMap<u32, Number> = BTreeMap::new();
    for entry in fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
    {
        let file = entry
            .expect("read db/migrations entry")
            .file_name()
            .to_string_lossy()
            .to_string();
        if file == "README.md" {
            continue;
        }
        let (stem, is_sql) = if let Some(stem) = file.strip_suffix(".sql") {
            (stem, true)
        } else if let Some(stem) = file.strip_suffix(".meta.toml") {
            (stem, false)
        } else {
            panic!("db/migrations holds only NNNN_name.sql and NNNN_name.meta.toml, found {file}");
        };
        let number = stem
            .get(..4)
            .and_then(|digits| digits.parse::<u32>().ok())
            .filter(|_| stem.as_bytes().get(4) == Some(&b'_') && stem.len() > 5)
            .unwrap_or_else(|| panic!("{file}: name must be NNNN_name with a four-digit number"));
        let slot = numbers.entry(number).or_default();
        if is_sql {
            slot.sql.push(stem.to_string());
        } else {
            slot.meta.push(stem.to_string());
        }
    }
    assert!(!numbers.is_empty(), "db/migrations holds no migrations");
    numbers
}

#[test]
fn each_number_names_exactly_one_migration() {
    for (number, files) in numbers() {
        let mut sql = files.sql.clone();
        sql.sort();
        assert!(
            !sql.is_empty(),
            "migration number {number:04} has {}.meta.toml but no .sql file",
            files.meta.join(".meta.toml, ")
        );
        assert!(
            sql.len() == 1,
            "migration number {number:04} is used by {} .sql files ({}); a number is \
             never reused, so renumber the newer one to the next free number on current main",
            sql.len(),
            sql.join(".sql, ") + ".sql"
        );
        let mut meta = files.meta.clone();
        meta.sort();
        assert!(
            meta.len() <= 1,
            "migration number {number:04} has {} .meta.toml files ({})",
            meta.len(),
            meta.join(".meta.toml, ") + ".meta.toml"
        );
        if let Some(stem) = meta.first() {
            assert_eq!(
                stem, &sql[0],
                "{stem}.meta.toml must sit beside {stem}.sql, but number {number:04} is {}.sql",
                sql[0]
            );
        }
    }
}

#[test]
fn numbers_run_from_one_with_no_gap() {
    for (index, number) in numbers().keys().enumerate() {
        let expected = u32::try_from(index).expect("few migrations") + 1;
        assert_eq!(
            *number, expected,
            "db/migrations numbers must run 0001, 0002, ... with no gap: found {number:04} \
             where {expected:04} was expected"
        );
    }
}
