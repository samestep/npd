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
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

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

/// What the batch build observed per drv: which ones nix actually attempted to
/// build (a build activity started) and how long each build activity took.
struct BatchInfo {
    attempted: HashSet<String>,
    durations: HashMap<String, f64>,
}

/// Build all of `drvs` (all outputs) in ONE nix invocation — nix schedules them
/// together with its own parallelism — while acting as a middleman: nix emits
/// `--log-format internal-json`, we forward it to `nom --json` for the live
/// tree and simultaneously parse build (`type:105`) start/stop events. That
/// gives us, per drv, whether it was *attempted* (start seen ⇒ a later failure
/// is its own, not a dependency's) and its build duration — neither of which a
/// plain batch build exposes. `--keep-going` so every drv is attempted.
fn batch_build(drvs: &[&str], force: bool) -> Result<BatchInfo> {
    let installables: Vec<String> = drvs.iter().map(|d| format!("{d}^*")).collect();
    let mut nix = Command::new("nix");
    nix.arg("build")
        .args(&installables)
        .args([
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
    let mut durations = HashMap::new();
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
                {
                    durations.insert(drv, t0.elapsed().as_secs_f64());
                }
            }
            _ => {}
        }
    }
    drop(nom_in); // EOF -> nom finishes rendering and exits
    let _ = nix.wait();
    let _ = nom.wait();
    Ok(BatchInfo {
        attempted,
        durations,
    })
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
        let info = batch_build(&drvs, force)?;
        let secs = start.elapsed().as_secs_f64();

        let built_map = build_outcomes(&drvs)?;
        let now = chrono::Utc::now().timestamp();
        for &i in &to_build {
            let t = &targets[i];
            let built = built_map.get(&t.drv_path).copied().unwrap_or(false);
            // A failed drv nix *attempted* (a build activity started) failed on
            // its own; one it never attempted was blocked by a failed dependency.
            let (outcome, label) = if built {
                (Outcome::Built, "built ")
            } else if info.attempted.contains(&t.drv_path) {
                (Outcome::Failed, "FAILED")
            } else {
                (Outcome::DepFailed, "dep-failed")
            };
            if built {
                root_drv(&t.drv_path, &cache)?;
            }
            let duration_s = info.durations.get(&t.drv_path).copied();
            store.add_observation(&Observation {
                drv_path: t.drv_path.clone(),
                source: Source::Local,
                outcome,
                when: now,
                system: Some(t.system.clone()),
                duration_s,
                cached: None,
                machine: Some(host.clone()),
                log_ref: None,
                build_id: None,
            })?;
            let dur = duration_s.map(|s| format!(" ({s:.0}s)")).unwrap_or_default();
            println!("  {label}  {} {}{dur}", t.system, t.attr);
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
