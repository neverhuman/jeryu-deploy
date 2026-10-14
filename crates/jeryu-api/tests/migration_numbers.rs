//! `db/migrations/` numbers are a sequence: one `.sql` file per number,
//! 0001 upwards with no gap, and any `.meta.toml` beside the `.sql` it
//! describes. Two branches cut from the same base can each take "the next"
//! number; this check fails the gate on the combined tree when that happens.
//!
//! The convention is written down in `db/migrations/README.md`, whose table
//! also names the store each migration belongs to; the shift store's rows
//! must match what its runner embeds (`jeryu_api::web::shift_store_migrations`).
//!
//! Only numbered `.sql` and `.meta.toml` files are judged. Anything else in
//! the directory (README.md, a subdirectory, an editor backup) is ignored.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const SHIFT_STORE: &str = "<data_dir>/shift.sqlite";
const RETIRE: &str = "a landed migration is never deleted or renumbered; to retire \
                      one, land a new migration that undoes it";

#[derive(Default)]
struct Number {
    sql: Vec<String>,
    meta: Vec<String>,
}

fn migrations_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../db/migrations")
}

fn numbers() -> BTreeMap<u32, Number> {
    let directory = migrations_directory();
    let mut numbers: BTreeMap<u32, Number> = BTreeMap::new();
    for entry in fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
    {
        let entry = entry.expect("read db/migrations entry");
        if !entry.path().is_file() {
            continue;
        }
        let file = entry.file_name().to_string_lossy().to_string();
        let (stem, is_sql) = if let Some(stem) = file.strip_suffix(".sql") {
            (stem, true)
        } else if let Some(stem) = file.strip_suffix(".meta.toml") {
            (stem, false)
        } else {
            continue;
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
    for slot in numbers.values_mut() {
        slot.sql.sort();
        slot.meta.sort();
    }
    assert!(!numbers.is_empty(), "db/migrations holds no migrations");
    numbers
}

#[test]
fn each_number_names_exactly_one_migration() {
    for (number, files) in numbers() {
        let (sql, meta) = (&files.sql, &files.meta);
        assert!(
            !sql.is_empty(),
            "migration number {number:04} has {}.meta.toml but no .sql file",
            meta.join(".meta.toml, ")
        );
        assert!(
            sql.len() == 1,
            "migration number {number:04} is used by {} .sql files ({}); renumber the one \
             your branch added, taking the next number from current origin/main",
            sql.len(),
            sql.join(".sql, ") + ".sql"
        );
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
             where {expected:04} was expected; {RETIRE}"
        );
    }
}

/// `(stem, store)` for each row of the README's migration table.
fn readme_rows() -> Vec<(String, String)> {
    let path = migrations_directory().join("README.md");
    let readme = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    readme
        .lines()
        .filter_map(|line| {
            let mut cells = line.split('|').map(str::trim).skip(1);
            let file = cells.next()?.strip_prefix('`')?.strip_suffix(".sql`")?;
            let store = cells.next()?.trim_matches('`');
            Some((file.to_string(), store.to_string()))
        })
        .collect()
}

#[test]
fn every_migration_names_its_store_in_the_readme() {
    let rows = readme_rows();
    for (number, files) in numbers() {
        for stem in &files.sql {
            let named = rows.iter().filter(|(file, _)| file == stem).count();
            assert_eq!(
                named, 1,
                "{stem}.sql (number {number:04}) needs exactly one row in \
                 db/migrations/README.md naming its store"
            );
        }
    }
    for (file, _) in &rows {
        assert!(
            migrations_directory().join(format!("{file}.sql")).is_file(),
            "db/migrations/README.md lists {file}.sql, which does not exist"
        );
    }
}

#[cfg(feature = "web")]
#[test]
fn the_shift_store_embeds_every_migration_the_readme_gives_it() {
    let mut expected: Vec<String> = readme_rows()
        .into_iter()
        .filter(|(_, store)| store == SHIFT_STORE)
        .map(|(file, _)| file)
        .collect();
    expected.sort();
    assert!(!expected.is_empty(), "no README row names {SHIFT_STORE}");
    assert_eq!(
        jeryu_api::web::shift_store_migrations(),
        expected,
        "the shift store's runner (web/shift/heartbeats.rs MIGRATIONS) must embed, in \
         order, every migration that db/migrations/README.md assigns to {SHIFT_STORE}"
    );
}
