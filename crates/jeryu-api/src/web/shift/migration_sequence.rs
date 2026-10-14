//! Numbering rules for the shift.sqlite migrations.
//!
//! The embedded list is a compile-time constant, so its shape (1, 2, 3, ...
//! with each name carrying its own `NNNN_`) is proved by tests, never at
//! startup: a numbering slip in the source fails the gate, not a running
//! server. `tests/migration_numbers.rs` checks the files on disk, and
//! [`check`] below checks the embedded list in the unit tests.
//!
//! What only a running store can know is what it already applied. A store
//! that applied a migration under a number the build now gives to another
//! migration (a branch renumbered after a dev store migrated) would otherwise
//! re-run that migration's DDL under its new number or report a checksum
//! change on the wrong file. [`check_recorded`] names the disagreement.

use std::collections::BTreeMap;

use rusqlite::Connection;

/// Compare the store's applied migrations with the build's list: every
/// recorded version must be in the list under the same name.
pub(super) fn check_recorded(
    conn: &Connection,
    migrations: &[(i64, &str, &str)],
) -> Result<(), String> {
    let embedded: BTreeMap<i64, &str> = migrations
        .iter()
        .map(|(version, name, _)| (*version, *name))
        .collect();
    let mut statement = conn
        .prepare("SELECT version, name FROM shift_schema_migrations ORDER BY version")
        .map_err(|err| err.to_string())?;
    let recorded = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|err| err.to_string())?;
    for row in recorded {
        let (version, applied) = row.map_err(|err| err.to_string())?;
        let now = match embedded.get(&version) {
            Some(name) if *name == applied => continue,
            Some(name) => (*name).to_string(),
            None => "no migration with that number".to_string(),
        };
        return Err(format!(
            "migration {version} was applied as {applied} but the build now has {now}; \
             a landed or applied migration is never renumbered: restore the old number \
             or reset the dev store"
        ));
    }
    Ok(())
}

/// The embedded list's shape: no version twice, no gap, no step backwards,
/// and every name opening with its `NNNN_` version.
#[cfg(test)]
pub(super) fn check(migrations: &[(i64, &str, &str)]) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for (index, (version, name, _)) in migrations.iter().enumerate() {
        if !seen.insert(*version) {
            return Err(format!(
                "migration version {version} is declared twice (again by {name}); \
                 renumber the one your branch added to the next free number on current \
                 origin/main"
            ));
        }
        let expected = i64::try_from(index).map_or(i64::MAX, |index| index + 1);
        if *version != expected {
            return Err(format!(
                "migration {name} has version {version} where {expected} was expected; \
                 versions run 1, 2, 3, ... with no gap and never go backwards"
            ));
        }
        let prefix = format!("{version:04}_");
        if !name.starts_with(&prefix) {
            return Err(format!(
                "migration {name} has version {version}, so its name must start with {prefix}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{check, check_recorded};
    use crate::web::shift::heartbeats::{MIGRATIONS, migrate};

    #[test]
    fn the_embedded_list_is_a_clean_sequence() {
        check(MIGRATIONS).expect("embedded migrations");
    }

    #[test]
    fn a_repeated_version_is_refused() {
        let error = check(&[(1, "0001_a", ""), (2, "0002_b", ""), (2, "0002_c", "")])
            .expect_err("duplicate version");
        assert!(error.contains("version 2 is declared twice"), "{error}");
        assert!(error.contains("0002_c"), "{error}");
    }

    #[test]
    fn a_gap_or_a_step_back_is_refused() {
        let gap = check(&[(1, "0001_a", ""), (3, "0003_c", "")]).expect_err("gap");
        assert!(gap.contains("version 3 where 2 was expected"), "{gap}");
        let back = check(&[(2, "0002_b", ""), (1, "0001_a", "")]).expect_err("backwards");
        assert!(back.contains("version 2 where 1 was expected"), "{back}");
    }

    #[test]
    fn a_name_that_disagrees_with_its_version_is_refused() {
        let error = check(&[(1, "0002_a", "")]).expect_err("mismatched name");
        assert!(error.contains("must start with 0001_"), "{error}");
    }

    fn store_with(rows: &[(i64, &str)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE shift_schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                applied_ms INTEGER NOT NULL
            )",
        )
        .unwrap();
        for (version, name) in rows {
            conn.execute(
                "INSERT INTO shift_schema_migrations VALUES (?1, ?2, '', 0)",
                rusqlite::params![version, name],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn a_store_that_applied_a_number_under_another_name_is_refused() {
        let conn = store_with(&[(1, "0001_a"), (2, "0002_renumbered_away")]);
        let error = check_recorded(&conn, &[(1, "0001_a", ""), (2, "0002_from_main", "")])
            .expect_err("renamed version");
        assert_eq!(
            error,
            "migration 2 was applied as 0002_renumbered_away but the build now has \
             0002_from_main; a landed or applied migration is never renumbered: restore \
             the old number or reset the dev store"
        );
    }

    #[test]
    fn a_store_that_applied_a_number_the_build_lacks_is_refused() {
        let conn = store_with(&[(1, "0001_a"), (3, "0003_gone")]);
        let error = check_recorded(&conn, &[(1, "0001_a", ""), (2, "0002_b", "")])
            .expect_err("unknown version");
        assert!(
            error.starts_with(
                "migration 3 was applied as 0003_gone but the build now has no migration \
                 with that number;"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_store_the_build_migrated_opens_again() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).expect("a second open agrees with what the first applied");
        check_recorded(&conn, MIGRATIONS).unwrap();
    }

    #[test]
    fn a_renumbered_store_fails_to_open_with_the_specific_message() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "UPDATE shift_schema_migrations SET name = '0014_outcome_ledger' WHERE version = 14",
            [],
        )
        .unwrap();
        let error = migrate(&conn).expect_err("renamed version 14");
        assert!(
            error.starts_with("migration 14 was applied as 0014_outcome_ledger but the build"),
            "{error}"
        );
    }
}
