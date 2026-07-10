//! The SQLite fact store: the append-only observation log, in
//! `~/.cache/nix-npd/npd.sqlite` (DESIGN.md §3–§4). Evals do *not* live here —
//! they're standalone files (see `eval.rs`), so this DB stays tiny and holds
//! only the small, index-worthy, append-only observation log.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::model::{Observation, Outcome, Source, TestJob};

// This schema is the only one npd has ever had, and it must stay that way:
// there is no migration code here by design (see CLAUDE.md). Change it freely
// and in place — the whole store is a re-derivable cache, so the remedy for an
// incompatible change is deleting `~/.cache/nix-npd`, never a compat shim.
const SCHEMA: &str = "
-- The append-only observation log (DESIGN.md §3): the build driver appends a
-- `local`/`cache` fact here per drv.
CREATE TABLE IF NOT EXISTS observation (
    id         INTEGER PRIMARY KEY,
    drv_path   TEXT    NOT NULL,
    source     TEXT    NOT NULL,
    outcome    TEXT    NOT NULL,
    when_      INTEGER NOT NULL,
    system     TEXT,
    duration_s REAL,
    machine    TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS observation_drv ON observation (drv_path);

-- The `--tests` passthru.tests eval cache (DESIGN.md §4, §6). A test's drv is a
-- pure function of (commit, system, profile, package-attr), so we cache per
-- package and reuse across reviews at a commit. `test_pkg` marks a package fully
-- evaluated (present even when it has zero tests, so a no-test package isn't
-- re-evaluated every run); `test_drv` holds each resolved `<pkg>.tests.<name>`
-- drv (a package may contribute zero rows). Full drv paths, like `observation`.
-- `broken` is the test's own meta-blocked bit (a test can be unsupported on this
-- system even when its package builds — an x86-only NixOS test on aarch64), so
-- it's stored per test, not inferred from the package.
CREATE TABLE IF NOT EXISTS test_pkg (
    commit_  TEXT NOT NULL,
    system   TEXT NOT NULL,
    profile  TEXT NOT NULL,
    pkg_attr TEXT NOT NULL,
    PRIMARY KEY (commit_, system, profile, pkg_attr)
) STRICT, WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS test_drv (
    commit_   TEXT NOT NULL,
    system    TEXT NOT NULL,
    profile   TEXT NOT NULL,
    pkg_attr  TEXT NOT NULL,
    test_attr TEXT NOT NULL,
    drv_path  TEXT NOT NULL,
    broken    INTEGER NOT NULL,
    PRIMARY KEY (commit_, system, profile, test_attr)
) STRICT, WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS test_drv_pkg ON test_drv (commit_, system, profile, pkg_attr);
";

fn source_str(s: Source) -> &'static str {
    match s {
        Source::Local => "local",
        Source::Cache => "cache",
    }
}

fn source_from(s: &str) -> Result<Source> {
    Ok(match s {
        "local" => Source::Local,
        "cache" => Source::Cache,
        other => anyhow::bail!("unknown observation source in store: {other:?}"),
    })
}

fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Built => "built",
        Outcome::Failed => "failed",
        Outcome::DepFailed => "dep-failed",
    }
}

fn outcome_from(s: &str) -> Result<Outcome> {
    Ok(match s {
        "built" => Outcome::Built,
        "failed" => Outcome::Failed,
        "dep-failed" => Outcome::DepFailed,
        other => anyhow::bail!("unknown observation outcome in store: {other:?}"),
    })
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).context("creating cache directory")?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // WAL: readers don't block the writer; better for a durable local store.
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.execute_batch(SCHEMA).context("initializing schema")?;
        Ok(Self { conn })
    }

    /// Append one observation to the log (never overwrites; DESIGN.md §3).
    pub fn add_observation(&mut self, o: &Observation) -> Result<()> {
        self.conn.execute(
            "INSERT INTO observation \
             (drv_path, source, outcome, when_, system, duration_s, machine) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                o.drv_path,
                source_str(o.source),
                outcome_str(o.outcome),
                o.when,
                o.system,
                o.duration_s,
                o.machine,
            ],
        )?;
        Ok(())
    }

    /// All observations for a derivation, oldest first.
    pub fn load_observations(&self, drv_path: &str) -> Result<Vec<Observation>> {
        Ok(self
            .load_observations_many(std::slice::from_ref(&drv_path))?
            .remove(drv_path)
            .unwrap_or_default())
    }

    /// Load observations for many drvs in one query (oldest first per drv). Drvs
    /// with no observations are simply absent from the map. This is how a report
    /// or build over a whole changed set stays a single round-trip to SQLite
    /// rather than one query per target.
    pub fn load_observations_many(
        &self,
        drv_paths: &[&str],
    ) -> Result<std::collections::HashMap<String, Vec<Observation>>> {
        let mut out: std::collections::HashMap<String, Vec<Observation>> =
            std::collections::HashMap::new();
        if drv_paths.is_empty() {
            return Ok(out);
        }
        // `WHERE drv_path IN (?,?,…)` with one placeholder per drv.
        let placeholders = std::iter::repeat_n("?", drv_paths.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT drv_path, source, outcome, when_, system, duration_s, machine \
             FROM observation WHERE drv_path IN ({placeholders}) ORDER BY when_, id",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(drv_paths.iter());
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<f64>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        for row in rows {
            let (drv_path, source, outcome, when, system, duration_s, machine) = row?;
            out.entry(drv_path.clone()).or_default().push(Observation {
                drv_path,
                source: source_from(&source)?,
                outcome: outcome_from(&outcome)?,
                when,
                system,
                duration_s,
                machine,
            });
        }
        Ok(out)
    }

    // --- the `--tests` passthru.tests cache (DESIGN.md §4, §6) ---------------

    /// Which of `pkgs` have already had their tests evaluated at this key (so a
    /// run need only `eval_tests` the rest). Absence means "never evaluated",
    /// distinct from "evaluated, has no tests" (present here, no `test_drv` rows).
    pub fn tests_cached_pkgs(
        &self,
        commit: &str,
        system: &str,
        profile: &str,
        pkgs: &[String],
    ) -> Result<std::collections::HashSet<String>> {
        let mut out = std::collections::HashSet::new();
        if pkgs.is_empty() {
            return Ok(out);
        }
        let placeholders = std::iter::repeat_n("?", pkgs.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT pkg_attr FROM test_pkg \
             WHERE commit_ = ?1 AND system = ?2 AND profile = ?3 AND pkg_attr IN ({placeholders})",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(
            [commit, system, profile]
                .into_iter()
                .chain(pkgs.iter().map(String::as_str)),
        );
        let rows = stmt.query_map(params, |r| r.get::<_, String>(0))?;
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Record a completed test eval of `pkgs` (the miss set) and its resulting
    /// `jobs`, in one transaction. Every package in `pkgs` gets a `test_pkg`
    /// marker (even those with no tests — so they're not re-evaluated); each job
    /// with a drv gets a `test_drv` row. Idempotent (`INSERT OR REPLACE`), so a
    /// re-run over the same key is harmless.
    pub fn cache_test_eval(
        &mut self,
        commit: &str,
        system: &str,
        profile: &str,
        pkgs: &[String],
        jobs: &[TestJob],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for pkg in pkgs {
            tx.execute(
                "INSERT OR REPLACE INTO test_pkg (commit_, system, profile, pkg_attr) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![commit, system, profile, pkg],
            )?;
        }
        for j in jobs {
            if let Some(drv) = &j.drv_path {
                tx.execute(
                    "INSERT OR REPLACE INTO test_drv \
                     (commit_, system, profile, pkg_attr, test_attr, drv_path, broken) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        commit,
                        system,
                        profile,
                        j.pkg_attr,
                        j.test_attr,
                        drv,
                        j.broken
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// All cached test drvs for `pkgs` at this key, as `test_attr → (drv_path,
    /// broken)` (only tests that resolved to a derivation). One query for the
    /// whole set.
    pub fn tests_drvs_for(
        &self,
        commit: &str,
        system: &str,
        profile: &str,
        pkgs: &[String],
    ) -> Result<std::collections::HashMap<String, (String, bool)>> {
        let mut out = std::collections::HashMap::new();
        if pkgs.is_empty() {
            return Ok(out);
        }
        let placeholders = std::iter::repeat_n("?", pkgs.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT test_attr, drv_path, broken FROM test_drv \
             WHERE commit_ = ?1 AND system = ?2 AND profile = ?3 AND pkg_attr IN ({placeholders})",
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(
            [commit, system, profile]
                .into_iter()
                .chain(pkgs.iter().map(String::as_str)),
        );
        let rows = stmt.query_map(params, |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })?;
        for row in rows {
            let (test_attr, drv_path, broken) = row?;
            out.insert(test_attr, (drv_path, broken));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observations_append_and_load() {
        let dir = std::env::temp_dir().join(format!("npd-obs-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npd.sqlite")).unwrap();

        assert!(s.load_observations("/nix/store/x.drv").unwrap().is_empty());

        let mk = |outcome, when| Observation {
            drv_path: "/nix/store/x.drv".into(),
            source: Source::Local,
            outcome,
            when,
            system: Some("aarch64-linux".into()),
            duration_s: Some(1.5),
            machine: Some("host".into()),
        };
        s.add_observation(&mk(Outcome::Failed, 100)).unwrap();
        s.add_observation(&mk(Outcome::Built, 200)).unwrap();

        let got = s.load_observations("/nix/store/x.drv").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].outcome, Outcome::Failed); // oldest first
        assert_eq!(got[1].outcome, Outcome::Built);
        assert_eq!(got[1].source, Source::Local);
        assert_eq!(got[1].duration_s, Some(1.5));
        // a different drv is independent
        assert!(s.load_observations("/nix/store/y.drv").unwrap().is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cache_round_trip_and_negative() {
        let dir = std::env::temp_dir().join(format!("npd-testcache-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut s = Store::open(&dir.join("npd.sqlite")).unwrap();
        let (c, sys, prof) = ("commitA", "aarch64-linux", "default");
        let pkgs = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

        // Nothing cached yet.
        assert!(
            s.tests_cached_pkgs(c, sys, prof, &pkgs(&["hello", "ripgrep"]))
                .unwrap()
                .is_empty()
        );

        // hello has two tests (one marked broken); ripgrep has none; one test
        // errored (no drv).
        let jobs = vec![
            TestJob {
                pkg_attr: "hello".into(),
                test_attr: "hello.tests.run".into(),
                drv_path: Some("/nix/store/a.drv".into()),
                broken: false,
            },
            TestJob {
                pkg_attr: "hello".into(),
                test_attr: "hello.tests.version".into(),
                drv_path: Some("/nix/store/b.drv".into()),
                broken: true,
            },
            TestJob {
                pkg_attr: "hello".into(),
                test_attr: "hello.tests.broken".into(),
                drv_path: None,
                broken: false,
            },
        ];
        s.cache_test_eval(c, sys, prof, &pkgs(&["hello", "ripgrep"]), &jobs)
            .unwrap();

        // Both packages are now marked evaluated — including the no-test one, so
        // it isn't re-evaluated (negative caching).
        let done = s
            .tests_cached_pkgs(c, sys, prof, &pkgs(&["hello", "ripgrep", "curl"]))
            .unwrap();
        assert!(done.contains("hello") && done.contains("ripgrep") && !done.contains("curl"));

        // hello resolves to its two drv'd tests (the errored one is not stored),
        // each carrying its own meta-blocked bit.
        let hd = s.tests_drvs_for(c, sys, prof, &pkgs(&["hello"])).unwrap();
        assert_eq!(hd.len(), 2);
        assert_eq!(
            hd.get("hello.tests.run"),
            Some(&("/nix/store/a.drv".to_string(), false))
        );
        assert_eq!(
            hd.get("hello.tests.version"),
            Some(&("/nix/store/b.drv".to_string(), true))
        );
        // ripgrep is cached-done but has no test drvs.
        assert!(
            s.tests_drvs_for(c, sys, prof, &pkgs(&["ripgrep"]))
                .unwrap()
                .is_empty()
        );
        // a different commit shares nothing.
        assert!(
            s.tests_cached_pkgs("commitB", sys, prof, &pkgs(&["hello"]))
                .unwrap()
                .is_empty()
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
