//! The observation-backed build driver (DESIGN.md §5): decide per target
//! (build / skip-known-ok / skip-cached / skip-known-failure), then build the
//! whole build set in ONE `nom build` invocation and attribute each drv's
//! outcome from a post-build output-validity check.
//!
//! This is the first writer to the observation log, and the reason it exists:
//! Nix remembers successful builds (the store), but *forgets failures* — so
//! without this, a known-failing derivation gets retried on every run.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};

use crate::eval;
use crate::hydra;
use crate::model::{BuildPolicy, Decision, Observation, Outcome, Source};
use crate::store::Store;

/// One derivation to consider building, with the attr/system it came from (for
/// reporting). Produced from either an explicit eval or a diff's changed set.
pub struct Target {
    pub attr: String,
    pub system: String,
    pub drv_path: String,
}

/// What happened to one target.
pub struct Built {
    pub attr: String,
    pub system: String,
    pub drv_path: String,
    pub decision: Decision,
    /// The build outcome, when `decision` was `Build` and this was not a dry run.
    pub outcome: Option<Outcome>,
}

/// The 32-char store-path hash component of a `/nix/store/<hash>-name[.drv]` path.
fn store_hash(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .and_then(|n| n.split('-').next())
        .unwrap_or(path)
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

/// Build all of `drvs` (all outputs) in a single `nom build` — one live build
/// tree, and nix schedules them together with its own parallelism. `--keep-going`
/// so every drv is attempted (needed to attribute per-drv outcomes afterward).
/// The exit code isn't used: outcomes come from the validity check.
fn batch_build(drvs: &[&str], force: bool) -> Result<()> {
    let installables: Vec<String> = drvs.iter().map(|d| format!("{d}^*")).collect();
    let mut cmd = Command::new("nom");
    cmd.arg("build")
        .args(&installables)
        .args(["--keep-going", "--extra-experimental-features", "nix-command"]);
    if force {
        // --recheck / --prefer-local: build from source even if valid/substitutable.
        cmd.arg("--rebuild");
    }
    cmd.status().context("running nom build (nix-output-monitor)")?;
    Ok(())
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

/// Root a (already-built) drv's outputs under the cache's gcroots so GC keeps
/// them. Instant — the drv is valid, so `nix build` just creates the symlink.
fn root_drv(drv: &str, cache: &Path) -> Result<()> {
    let gcroot = cache.join("gcroots").join(store_hash(drv));
    fs::create_dir_all(gcroot.parent().unwrap()).context("creating gcroots dir")?;
    Command::new("nix")
        .args(["build", &format!("{drv}^*"), "--out-link"])
        .arg(&gcroot)
        .args(["--extra-experimental-features", "nix-command"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("rooting built output")?;
    Ok(())
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
pub fn build_targets(
    targets: &[Target],
    policy: BuildPolicy,
    dry_run: bool,
) -> Result<Vec<Built>> {
    let mut store = Store::open(&eval::db_path()?)?;
    let cache = eval::cache_root()?;
    let host = hostname();
    // --recheck / --prefer-local force a genuine local build; otherwise a cached
    // (substitutable) output means we needn't build at all.
    let force = policy.recheck || policy.prefer_local;

    // Pass 1: decide per target. Record/print skips now; collect the build set.
    let mut results: Vec<Built> = Vec::with_capacity(targets.len());
    let mut to_build: Vec<usize> = Vec::new();
    for (i, t) in targets.iter().enumerate() {
        let observations = store.load_observations(&t.drv_path)?;
        // Only probe the cache when it could change the decision (not when forcing).
        let substitutable = !force && hydra::in_cache(&t.drv_path);
        let decision = policy.decide(&observations, substitutable);
        match decision {
            Decision::Build if dry_run => println!("  would build           {} {}", t.system, t.attr),
            Decision::Build => to_build.push(i),
            Decision::SkipOk => {
                let has_local_built = observations
                    .iter()
                    .any(|o| o.source == Source::Local && o.outcome == Outcome::Built);
                if substitutable && !has_local_built {
                    // In the cache, not built here — record a Cache fact (deduped)
                    // so the report shows `C`, never a bogus `L`.
                    let known = observations
                        .iter()
                        .any(|o| o.source == Source::Cache && o.outcome == Outcome::Built);
                    if !known {
                        store.add_observation(&Observation {
                            drv_path: t.drv_path.clone(),
                            source: Source::Cache,
                            outcome: Outcome::Built,
                            when: chrono::Utc::now().timestamp(),
                            system: Some(t.system.clone()),
                            duration_s: None,
                            cached: Some(true),
                            machine: None,
                            log_ref: None,
                            build_id: None,
                        })?;
                    }
                    println!("  skip (in binary cache) {} {}", t.system, t.attr);
                } else {
                    println!("  skip (known ok)        {} {}", t.system, t.attr);
                }
            }
            Decision::SkipFail => println!("  skip (known failure)   {} {}", t.system, t.attr),
        }
        results.push(Built {
            attr: t.attr.clone(),
            system: t.system.clone(),
            drv_path: t.drv_path.clone(),
            decision,
            outcome: None,
        });
    }

    // Pass 2: one nom build for the whole set; Pass 3: attribute + record + root.
    if !to_build.is_empty() {
        let drvs: Vec<&str> = to_build.iter().map(|&i| targets[i].drv_path.as_str()).collect();
        println!("building {} derivation(s)…", drvs.len());
        let start = Instant::now();
        batch_build(&drvs, force)?;
        let secs = start.elapsed().as_secs_f64();

        let outcomes = build_outcomes(&drvs)?;
        let now = chrono::Utc::now().timestamp();
        for &i in &to_build {
            let t = &targets[i];
            let built = outcomes.get(&t.drv_path).copied().unwrap_or(false);
            let outcome = if built { Outcome::Built } else { Outcome::Failed };
            if built {
                root_drv(&t.drv_path, &cache)?;
            }
            store.add_observation(&Observation {
                drv_path: t.drv_path.clone(),
                source: Source::Local,
                outcome,
                when: now,
                system: Some(t.system.clone()),
                duration_s: None, // one batch build; per-drv time isn't separable
                cached: None,
                machine: Some(host.clone()),
                log_ref: None,
                build_id: None,
            })?;
            println!(
                "  {}  {} {}",
                if built { "built " } else { "FAILED" },
                t.system,
                t.attr
            );
            results[i].outcome = Some(outcome);
        }
        println!("(built set finished in {secs:.0}s)");
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_hash_extracts_the_hash() {
        assert_eq!(
            store_hash("/nix/store/izk77azi9bcldnpdw4c62hc637q8xm27-hello-2.12.3.drv"),
            "izk77azi9bcldnpdw4c62hc637q8xm27"
        );
        assert_eq!(
            store_hash("/nix/store/qpp9968dpkv1c755nk13mrkrzpsvah18-hello-2.12.3"),
            "qpp9968dpkv1c755nk13mrkrzpsvah18"
        );
    }
}
