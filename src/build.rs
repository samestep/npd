//! The observation-backed build driver (DESIGN.md §5): decide per target
//! (build / skip-known-ok / skip-cached / skip-known-failure), then build the
//! whole build set in ONE `nom build` invocation. Each drv's outcome is
//! recorded the moment its build activity stops — so an interrupted (^C) batch
//! keeps every fact observed so far — and drvs nix never attempted are
//! attributed from a post-build output-validity check.
//!
//! This is the first writer to the observation log, and the reason it exists:
//! Nix remembers successful builds (the store), but *forgets failures* — so
//! without this, a known-failing derivation gets retried on every run.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cache;
use crate::eval;
use crate::model::{BuildPolicy, Decision, Observation, Outcome, Source};
use crate::store::Store;

/// One derivation to consider building, with the attr/system it came from (for
/// reporting). Produced from either an explicit eval or a diff's changed set.
pub struct Target {
    pub attr: String,
    pub system: String,
    pub drv_path: String,
    /// Marked broken/unsupported/insecure in meta — skipped by the default
    /// policy (`BuildPolicy::build_broken` overrides).
    pub broken: bool,
}

fn hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// nix internal-json log event (only the fields we use).
#[derive(Deserialize)]
struct NixEvent {
    action: String,
    id: Option<u64>,
    #[serde(rename = "type")]
    typ: Option<i64>,
    #[serde(default)]
    fields: Vec<serde_json::Value>,
}

/// The `actBuild` activity type in nix's internal-json log.
const ACT_BUILD: i64 = 105;

/// Build all of `drvs` (all outputs) in ONE nix invocation — nix schedules them
/// together with its own parallelism — while acting as a middleman: nix emits
/// `--log-format internal-json`, we forward it to `nom --json` for the live
/// tree and simultaneously parse build (`type:105`) start/stop events. That
/// gives us, per drv, whether it was *attempted* (start seen ⇒ a later failure
/// is its own, not a dependency's) and its build duration — neither of which a
/// plain batch build exposes. `--keep-going` so every drv is attempted.
///
/// `on_finish(drv, secs)` fires as each of `drvs`'s build activities stops.
/// Nix registers a successful build's outputs *before* emitting the stop event
/// (both the local and build-hook goals `registerValidPaths` before destroying
/// the `actBuild` Activity — nix 2.34 `derivation-building-goal.cc`), so the
/// callback can attribute the outcome from output validity right away.
///
/// Returns every drv nix attempted (build started), dependencies included.
/// (Nix keeps the build log itself under `/nix/var/log/nix/drvs`; `nix log
/// <drv>` retrieves it, so npd doesn't duplicate it.)
fn batch_build(
    drvs: &[&str],
    force: bool,
    mut on_finish: impl FnMut(&str, f64) -> Result<()>,
) -> Result<HashSet<String>> {
    let requested: HashSet<&str> = drvs.iter().copied().collect();
    let installables: Vec<String> = drvs.iter().map(|d| format!("{d}^*")).collect();
    let mut nix = Command::new("nix");
    nix.arg("build").args(&installables).args([
        "--keep-going",
        "--log-format",
        "internal-json",
        "-v",
        "--extra-experimental-features",
        "nix-command",
    ]);
    if force {
        nix.arg("--rebuild");
    }
    let mut nix = nix
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning nix build")?;
    let mut nom = Command::new("nom")
        .arg("--json")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawning nom --json (nix-output-monitor)")?;

    let log = BufReader::new(nix.stderr.take().expect("stderr is piped"));
    let mut nom_in = nom.stdin.take().expect("stdin is piped");
    let mut attempted = HashSet::new();
    let mut starts: HashMap<u64, (String, Instant)> = HashMap::new();

    for line in log.lines() {
        let line = line.context("reading nix build log")?;
        // Forward the raw internal-json line to nom, which renders the tree.
        let _ = writeln!(nom_in, "{line}");
        let Some(rest) = line.strip_prefix("@nix ") else {
            continue;
        };
        let Ok(ev) = serde_json::from_str::<NixEvent>(rest) else {
            continue;
        };
        match ev.action.as_str() {
            "start" if ev.typ == Some(ACT_BUILD) => {
                if let (Some(id), Some(drv)) = (ev.id, ev.fields.first().and_then(|v| v.as_str())) {
                    attempted.insert(drv.to_string());
                    starts.insert(id, (drv.to_string(), Instant::now()));
                }
            }
            "stop" => {
                if let Some(id) = ev.id
                    && let Some((drv, t0)) = starts.remove(&id)
                    && requested.contains(drv.as_str())
                {
                    on_finish(&drv, t0.elapsed().as_secs_f64())?;
                }
            }
            _ => {}
        }
    }
    drop(nom_in); // EOF -> nom finishes rendering and exits
    let _ = nix.wait();
    let _ = nom.wait();
    Ok(attempted)
}

/// The realised output paths of a derivation.
fn drv_outputs(drv: &str) -> Result<Vec<String>> {
    let out = Command::new("nix-store")
        .args(["--query", "--outputs", drv])
        .output()
        .context("running nix-store --query --outputs")?;
    if !out.status.success() {
        bail!("nix-store --query --outputs {drv} failed");
    }
    Ok(lines(&out.stdout))
}

/// Which of `paths` are NOT valid in the local store (i.e. weren't built).
fn invalid_paths(paths: &[String]) -> Result<HashSet<String>> {
    if paths.is_empty() {
        return Ok(HashSet::new());
    }
    // Prints the invalid subset; exits non-zero when some are invalid, which is
    // expected — parse stdout regardless.
    let out = Command::new("nix-store")
        .args(["--check-validity", "--print-invalid"])
        .args(paths)
        .output()
        .context("running nix-store --check-validity")?;
    Ok(lines(&out.stdout).into_iter().collect())
}

/// Did this drv's build succeed — are all its outputs valid in the local
/// store? Sound at stop-event time; see `batch_build`.
fn drv_built(drv: &str) -> Result<bool> {
    let outs = drv_outputs(drv)?;
    Ok(!outs.is_empty() && invalid_paths(&outs)?.is_empty())
}

/// Map each built drv to whether all its outputs are now valid (i.e. it built).
fn build_outcomes(drvs: &[&str]) -> Result<HashMap<String, bool>> {
    let mut per_drv: Vec<(String, Vec<String>)> = Vec::new();
    let mut all = Vec::new();
    for &d in drvs {
        let outs = drv_outputs(d)?;
        all.extend(outs.iter().cloned());
        per_drv.push((d.to_string(), outs));
    }
    let invalid = invalid_paths(&all)?;
    Ok(per_drv
        .into_iter()
        .map(|(d, outs)| {
            let built = !outs.is_empty() && outs.iter().all(|o| !invalid.contains(o));
            (d, built)
        })
        .collect())
}

fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// For each target, consult `policy` against the observation log; then build the
/// whole build set at once. With `dry_run`, decisions are computed and printed
/// but nothing is built or recorded.
pub fn build_targets(targets: &[Target], policy: BuildPolicy, dry_run: bool) -> Result<()> {
    build_targets_at(&eval::db_path()?, targets, policy, dry_run)
}

/// [`build_targets`] against an explicit observation DB (separable for tests).
fn build_targets_at(
    db: &std::path::Path,
    targets: &[Target],
    policy: BuildPolicy,
    dry_run: bool,
) -> Result<()> {
    let mut store = Store::open(db)?;
    let host = hostname();
    // --recheck / --prefer-local force a genuine local build; otherwise a cached
    // (substitutable) output means we needn't build at all.
    let force = policy.recheck || policy.prefer_local;

    // One SQLite round-trip for every target's history, rather than one query
    // each — so an all-known set costs a single query, not N.
    let drv_refs: Vec<&str> = targets.iter().map(|t| t.drv_path.as_str()).collect();
    let obs_by_drv = store.load_observations_many(&drv_refs)?;
    let obs_of = |drv: &str| obs_by_drv.get(drv).map(Vec::as_slice).unwrap_or(&[]);

    let cache_built = |drv: &str| {
        obs_of(drv)
            .iter()
            .any(|o| o.source == Source::Cache && o.outcome == Outcome::Built)
    };
    let local_built = |drv: &str| {
        obs_of(drv)
            .iter()
            .any(|o| o.source == Source::Local && o.outcome == Outcome::Built)
    };
    // We only probe the cache for drvs we have *no fact* about — a probe can only
    // change the decision there. A drv with any local observation is already
    // decided (built → skip; failed/blocked → skip-fail, since a local failure
    // outranks cache presence anyway), and a recorded cache hit is decided too.
    // This is what keeps a re-run of an unchanged report near-instant: we don't
    // re-probe (HTTP + `nix-store`) the failures every time. Probes that do run
    // run concurrently (see `cache::in_cache_many`).
    let has_fact =
        |drv: &str| obs_of(drv).iter().any(|o| o.source == Source::Local) || cache_built(drv);
    // A broken target the policy will skip anyway isn't worth an HTTP probe.
    let skipped_broken = |t: &Target| t.broken && !policy.build_broken;
    let mut to_probe: Vec<String> = Vec::new();
    if !force {
        let mut seen = HashSet::new();
        for t in targets {
            if !has_fact(&t.drv_path) && !skipped_broken(t) && seen.insert(t.drv_path.clone()) {
                to_probe.push(t.drv_path.clone());
            }
        }
    }
    let probed = cache::in_cache_many(&to_probe);
    let substitutable =
        |drv: &str| !force && (cache_built(drv) || probed.get(drv).copied().unwrap_or(false));

    // Pass 1: decide per target. Skips are silent — a fully-cached run must print
    // nothing; dry-run still lists each would-build target, since that's its point.
    let now = chrono::Utc::now().timestamp();
    let mut to_build: Vec<usize> = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let observations = obs_of(&t.drv_path);
        let sub = substitutable(&t.drv_path);
        match policy.decide(observations, sub, t.broken) {
            Decision::Build if dry_run => {
                println!("  would build           {} {}", t.system, t.attr)
            }
            Decision::Build => to_build.push(i),
            // Substitutable but not built here: record a Cache fact (deduped) so
            // the report reflects it and a re-run needn't probe the cache again.
            Decision::SkipOk => {
                if sub && !local_built(&t.drv_path) && !cache_built(&t.drv_path) {
                    store.add_observation(&Observation {
                        drv_path: t.drv_path.clone(),
                        source: Source::Cache,
                        outcome: Outcome::Built,
                        when: now,
                        system: Some(t.system.clone()),
                        duration_s: None,
                        machine: None,
                    })?;
                }
            }
            Decision::SkipFail | Decision::SkipBroken => {}
        }
    }

    // Pass 2: one nom build for the whole set, recording each drv's outcome the
    // moment its build activity stops — its outputs' validity at that instant is
    // the build's own result (see `batch_build`). Recording incrementally is
    // what makes ^C mid-batch safe: every fact observed so far is already
    // committed, so only in-flight and never-started builds cost anything on
    // the next run. (Nix keeps the build log itself; `nix log <drv>` gets it.)
    if !to_build.is_empty() {
        let drvs: Vec<&str> = to_build
            .iter()
            .map(|&i| targets[i].drv_path.as_str())
            .collect();
        // Several targets can share a drv (aliased attrs); record it once.
        let system_of: HashMap<&str, &str> = to_build
            .iter()
            .map(|&i| (targets[i].drv_path.as_str(), targets[i].system.as_str()))
            .collect();
        let mut recorded: HashMap<String, Outcome> = HashMap::new();
        let attempted = batch_build(&drvs, force, |drv, secs| {
            let outcome = if drv_built(drv)? {
                Outcome::Built
            } else {
                Outcome::Failed
            };
            store.add_observation(&Observation {
                drv_path: drv.to_string(),
                source: Source::Local,
                outcome,
                when: chrono::Utc::now().timestamp(),
                system: system_of.get(drv).copied().map(str::to_string),
                duration_s: Some(secs),
                machine: Some(host.clone()),
            })?;
            recorded.insert(drv.to_string(), outcome);
            Ok(())
        })?;

        // Pass 3: attribute the drvs that had no build activity — a drv nix
        // *attempted* failed on its own; one it never attempted either had valid
        // outputs already (substituted, or a prior interrupted run built it) or
        // was blocked by a failed dependency. No per-target result lines: nom's
        // tree already showed each build's fate, and the report has the rest.
        let leftover: Vec<&str> = drvs
            .iter()
            .copied()
            .filter(|d| !recorded.contains_key(*d))
            .collect::<HashSet<&str>>()
            .into_iter()
            .collect();
        let built_map = build_outcomes(&leftover)?;
        let now = chrono::Utc::now().timestamp();
        for &drv in &leftover {
            let outcome = if built_map.get(drv).copied().unwrap_or(false) {
                Outcome::Built
            } else if attempted.contains(drv) {
                Outcome::Failed
            } else {
                Outcome::DepFailed
            };
            store.add_observation(&Observation {
                drv_path: drv.to_string(),
                source: Source::Local,
                outcome,
                when: now,
                system: system_of.get(drv).copied().map(str::to_string),
                duration_s: None,
                machine: Some(host.clone()),
            })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Instantiate a nix expression, returning its .drv path.
    fn instantiate(expr: &str, attr: &str) -> String {
        let out = Command::new("nix-instantiate")
            .args(["--expr", expr, "-A", attr])
            .output()
            .expect("running nix-instantiate");
        assert!(
            out.status.success(),
            "nix-instantiate -A {attr} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// End-to-end against real nix (hence ignored; `cargo test -- --ignored`):
    /// build a set with a fast failure, a slow success, and a drv blocked by the
    /// failure. Asserts the attribution of all three outcomes AND the property
    /// that makes ^C safe: the failure's observation is committed to SQLite
    /// while the batch is still building, not after it finishes.
    #[test]
    #[ignore = "builds real derivations via nix; needs nix, nom, and ~10s"]
    fn records_outcomes_while_batch_still_building() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("npd-build-test-{nonce}"));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("npd.sqlite");

        // Nonce'd names so nothing is valid in the store from a previous run.
        let expr = format!(
            r#"let
                 mk = name: cmd: derivation {{
                   name = name; system = builtins.currentSystem;
                   builder = "/bin/sh"; args = ["-c" cmd];
                 }};
                 fail = mk "npd-test-fail-{nonce}" "exit 1";
                 # Spin on shell builtins (~10s): the sandbox has no `sleep`
                 # (PATH is /path-not-set), and the delay must outlast the poll
                 # below that watches for the failure's row.
                 slow = mk "npd-test-slow-{nonce}"
                   "i=0; while [ $i -lt 15000000 ]; do i=$((i+1)); done; echo ok > $out";
                 blocked = mk "npd-test-blocked-{nonce}" "cat ${{fail}} > $out";
               in {{ inherit fail slow blocked; }}"#
        );
        let fail = instantiate(&expr, "fail");
        let slow = instantiate(&expr, "slow");
        let blocked = instantiate(&expr, "blocked");

        let targets: Vec<Target> = [("fail", &fail), ("slow", &slow), ("blocked", &blocked)]
            .into_iter()
            .map(|(attr, drv)| Target {
                attr: attr.to_string(),
                system: "testsys".to_string(),
                drv_path: drv.clone(),
                broken: false,
            })
            .collect();
        let db2 = db.clone();
        let builder = std::thread::spawn(move || {
            build_targets_at(&db2, &targets, BuildPolicy::default(), false)
        });

        // The failure is near-instant, the success sleeps 8s; its Failed row
        // must land while the batch (and the thread driving it) still runs.
        let mut seen_mid_batch = false;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !builder.is_finished() {
            // Concurrent open can transiently fail (writer holds the lock).
            if let Ok(s) = Store::open(&db)
                && let Ok(obs) = s.load_observations(&fail)
                && obs.iter().any(|o| o.outcome == Outcome::Failed)
            {
                seen_mid_batch = !builder.is_finished();
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(
            seen_mid_batch,
            "failure was not recorded while the batch was still building"
        );

        builder.join().unwrap().unwrap();

        // Every outcome is recovered from the observation log — the same ground
        // truth the production path renders from. Each drv is observed exactly
        // once: the failure and the slow success from their own build activity,
        // the blocked drv from the post-batch output-validity sweep.
        let s = Store::open(&db).unwrap();
        let obs_of = |drv: &str| {
            let obs = s.load_observations(drv).unwrap();
            assert_eq!(obs.len(), 1, "exactly one local observation per drv");
            obs.into_iter().next().unwrap()
        };
        assert_eq!(obs_of(&fail).outcome, Outcome::Failed);
        assert_eq!(obs_of(&slow).outcome, Outcome::Built);
        assert_eq!(obs_of(&blocked).outcome, Outcome::DepFailed);

        // The incrementally-recorded facts carry a duration and the system.
        let fail_obs = obs_of(&fail);
        assert_eq!(fail_obs.source, Source::Local);
        assert!(fail_obs.duration_s.is_some());
        assert_eq!(fail_obs.system.as_deref(), Some("testsys"));

        let _ = fs::remove_dir_all(&dir);
    }
}
