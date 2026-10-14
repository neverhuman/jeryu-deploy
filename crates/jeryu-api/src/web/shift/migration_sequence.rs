//! The shift.sqlite migration list must run 1, 2, 3, ... with each name
//! carrying its own zero-padded version. Two branches cut from the same base
//! can each add "the next" migration; once both land, the list holds one
//! version twice. `apply_migrations` checks the list before it touches the
//! store, so such a binary refuses to start instead of skipping or
//! misrecording a migration.

use std::collections::BTreeSet;

/// Check the embedded migration list: no version twice, no gap, no step
/// backwards, and every name opening with its `NNNN_` version.
pub(super) fn check(migrations: &[(i64, &str, &str)]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for (index, (version, name, _)) in migrations.iter().enumerate() {
        if !seen.insert(*version) {
            return Err(format!(
                "migration version {version} is declared twice (again by {name}); \
                 renumber the newer one to the next free number on current main"
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
    use std::fs;
    use std::path::Path;

    use super::check;
    use crate::web::shift::heartbeats::MIGRATIONS;

    #[test]
    fn the_embedded_list_is_a_clean_sequence() {
        check(MIGRATIONS).expect("embedded migrations");
    }

    #[test]
    fn every_sql_file_is_embedded_under_its_own_version() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../db/migrations");
        let mut stems: Vec<String> = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
            .map(|entry| entry.expect("read db/migrations entry").file_name())
            .filter_map(|name| {
                name.to_string_lossy()
                    .strip_suffix(".sql")
                    .map(str::to_string)
            })
            .collect();
        stems.sort();
        let embedded: Vec<&str> = MIGRATIONS.iter().map(|(_, name, _)| *name).collect();
        assert_eq!(
            stems, embedded,
            "every db/migrations/*.sql file is embedded in heartbeats.rs MIGRATIONS, in order"
        );
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
}
