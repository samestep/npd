//! npd — a persistent fact store for iterating on nixpkgs changes.
//!
//! See DESIGN.md for the architecture. The pure data model lives in [`model`];
//! `npd` is a single command that evaluates a `base → head` change, builds
//! whatever the changed set needs, and renders a Markdown report.

mod build;
mod cache;
mod eval;
mod evalfile;
mod model;
mod paths;
mod report;
mod store;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::process::Command as Proc;

use anyhow::{Context, Result, bail};
use clap::Parser;

use crate::model::BuildPolicy;

#[derive(Parser)]
#[command(
    name = "npd",
    version,
    about = "A persistent fact store for iterating on nixpkgs changes"
)]
struct Cli {
    /// Base revision (default: merge-base of head and `master`).
    base: Option<String>,
    /// Head revision (default: `HEAD`).
    head: Option<String>,
    /// nixpkgs clone to resolve the commits in (default: current directory).
    #[arg(long)]
    nixpkgs: Option<PathBuf>,
    /// Systems to report on (repeatable); defaults to the host system.
    #[arg(long)]
    system: Vec<String>,
    /// Don't build; render only from facts already in the log (may show `❓`).
    #[arg(long)]
    no_build: bool,
    /// Rebuild even a previously-succeeded drv (suspect a flaky success).
    #[arg(long)]
    recheck: bool,
    /// Re-attempt a previously-failed drv (expect it might pass now).
    #[arg(long)]
    retry: bool,
    /// Ignore a substitutable (cached) success; require a genuine local build.
    #[arg(long)]
    prefer_local: bool,
    /// For each changed package, also evaluate and build its `passthru.tests`
    /// (on both sides), classifying each test's `base → head` delta like any
    /// other attr. Ported from nixpkgs-review's `--tests` (#397).
    #[arg(long)]
    tests: bool,
    /// Also build packages marked broken/unsupported/insecure (skipped and
    /// reported as 🚧 by default, like nixpkgs-review).
    #[arg(long)]
    build_broken: bool,
    /// Everything on: implies --tests and --build-broken.
    #[arg(long)]
    max: bool,
    /// Eval-scheduler knobs; each unset flag is auto-sized from the machine's
    /// cores and total RAM (see `eval::eval_slots`).
    #[command(flatten)]
    eval: eval::EvalOpts,
}

/// The host Nix system double, e.g. `aarch64-linux`.
fn host_system() -> String {
    let arch = std::env::consts::ARCH; // e.g. "aarch64", "x86_64"
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other, // "linux"
    };
    format!("{arch}-{os}")
}

/// The nixpkgs clone to operate on: `--nixpkgs` if given, else the current
/// directory (assumed to be the root of a nixpkgs checkout). Canonicalized,
/// because a relative `--nixpkgs` that `git -C` accepts would be embedded
/// verbatim in the eval expression, where `builtins.fetchGit` needs an
/// absolute path.
fn resolve_repo(nixpkgs: Option<PathBuf>) -> Result<PathBuf> {
    let p = match nixpkgs {
        Some(p) => p,
        None => std::env::current_dir()
            .context("could not determine the current directory; pass --nixpkgs <path>")?,
    };
    p.canonicalize()
        .with_context(|| format!("resolving nixpkgs path {}", p.display()))
}

fn resolve_systems(system: Vec<String>) -> Vec<String> {
    if system.is_empty() {
        vec![host_system()]
    } else {
        system
    }
}

/// `git merge-base base head` in `repo`.
fn git_merge_base(repo: &std::path::Path, base: &str, head: &str) -> Result<String> {
    let out = Proc::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", base, head])
        .output()
        .context("running git merge-base")?;
    if !out.status.success() {
        bail!(
            "git merge-base {base} {head} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Resolve a revision (ref, short/full sha, tag, `HEAD~1`, …) to a full commit
/// sha, so callers can use friendly names even though `fetchGit` needs a rev.
fn resolve_commit(repo: &std::path::Path, rev: &str) -> Result<String> {
    let out = Proc::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{rev}^{{commit}}"))
        .output()
        .context("running git rev-parse")?;
    if !out.status.success() {
        bail!("cannot resolve revision {rev:?} in {}", repo.display());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Resolve report revisions with ergonomic defaults: head defaults to `HEAD`,
/// base to the merge-base of head and `master` (the fork point of this branch).
fn resolve_base_head(
    repo: &std::path::Path,
    base: Option<String>,
    head: Option<String>,
) -> Result<(String, String)> {
    let head = resolve_commit(repo, &head.unwrap_or_else(|| "HEAD".to_string()))?;
    let base = match base {
        Some(b) => resolve_commit(repo, &b)?,
        None => git_merge_base(repo, "master", &head)
            .context("no base given and could not merge-base with `master`; pass one explicitly")?,
    };
    Ok((base, head))
}

/// Test drvs for `pkgs` at one revision, via the per-package SQLite cache: look
/// up which packages are already evaluated, `eval_tests` only the misses, persist
/// them, then return `test_attr → (drv, broken)` for the whole set. A fully-cached
/// call runs no `nix-eval-jobs` — just two queries — so a re-run stays instant; a
/// package evaluated in any prior review at this commit is reused for free.
fn cached_test_drvs(
    store: &mut store::Store,
    repo: &std::path::Path,
    commit: &str,
    system: &str,
    pkgs: &[String],
) -> Result<std::collections::HashMap<String, (String, bool)>> {
    let done = store.tests_cached_pkgs(commit, system, pkgs)?;
    let misses: Vec<String> = pkgs
        .iter()
        .filter(|p| !done.contains(*p))
        .cloned()
        .collect();
    if !misses.is_empty() {
        let jobs = eval::eval_tests(repo, commit, system, &misses)?;
        store.cache_test_eval(commit, system, &misses, &jobs)?;
    }
    store.tests_drvs_for(commit, system, pkgs)
}

/// Flatten the per-system changed sets into build targets: every side's drv,
/// deduped per `(system, drv)`.
///
/// Several `(attr, side)` rows can share one drv with *different* meta-blocked
/// bits — aliases where only some variants are marked (on darwin `ollama-cuda`
/// shares `ollama`'s drv but is marked unsupported), or a meta-only unmarking
/// (a PR deleting `meta.broken` leaves the drv identical on both sides with
/// the bit flipped). The marking is a property of the *attr*, not the recipe,
/// so the deduped target is broken only if EVERY row wanting this drv is
/// marked: any unmarked row is a legitimate request to build it.
fn assemble_targets(
    per_system_changed: &[(String, Vec<evalfile::ChangedAttr>)],
) -> Vec<build::Target> {
    let mut targets: Vec<build::Target> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    for (sys, changed) in per_system_changed {
        for c in changed {
            let sides = [(&c.base_drv, c.base_broken), (&c.head_drv, c.head_broken)];
            for (drv, broken) in sides {
                let Some(drv) = drv else { continue };
                match index.entry((sys.clone(), drv.clone())) {
                    Entry::Occupied(e) => {
                        let t = &mut targets[*e.get()];
                        t.broken = t.broken && broken;
                    }
                    Entry::Vacant(e) => {
                        e.insert(targets.len());
                        targets.push(build::Target {
                            system: sys.clone(),
                            drv_path: drv.clone(),
                            broken,
                        });
                    }
                }
            }
        }
    }
    targets
}

fn run(cli: Cli) -> Result<()> {
    // --max is simply "everything on".
    let tests = cli.tests || cli.max;
    let build_broken = cli.build_broken || cli.max;
    let policy = BuildPolicy {
        recheck: cli.recheck,
        retry: cli.retry,
        prefer_local: cli.prefer_local,
        build_broken,
    };
    let opts = cli.eval;
    let repo = resolve_repo(cli.nixpkgs)?;
    let (base, head) = resolve_base_head(&repo, cli.base, cli.head)?;
    let systems = resolve_systems(cli.system);

    eval::eval_two(&repo, &base, &head, &systems, opts)?;

    // The changed set per system — each attr's drv + meta-blocked bit per side —
    // from a linear merge of the two sorted eval files. Computed once, reused
    // for build+render.
    let mut per_system_changed: Vec<(String, Vec<evalfile::ChangedAttr>)> = Vec::new();
    for sys in &systems {
        let changed = evalfile::changed_set(&base, &head, sys)?;
        per_system_changed.push((sys.clone(), changed));
    }

    // --tests: expand each system's changed set with the changed packages'
    // `passthru.tests`. We resolve the tests on *both* sides (through the
    // per-package SQLite cache — see `cached_test_drvs`) and keep a test as a
    // changed attr only when its drv actually differs base→head — exactly
    // `changed_set`'s own semantics, so the test rows classify (regression /
    // fixed / new / …) like every other attr. A side where the package is
    // marked broken contributes no tests (a test drv depends on the package,
    // so building it would build the broken package) unless --build-broken.
    if tests {
        let mut store = store::Store::open(&paths::db_path()?)?;
        for (sys, changed) in per_system_changed.iter_mut() {
            let names_on = |unbroken: fn(&evalfile::ChangedAttr) -> bool| -> Vec<String> {
                let mut v: Vec<String> = changed
                    .iter()
                    .filter(|c| build_broken || unbroken(c))
                    .map(|c| c.attr.clone())
                    .collect();
                v.sort();
                v.dedup();
                v
            };
            let base_names = names_on(|c| !c.base_broken);
            let head_names = names_on(|c| !c.head_broken);
            let bmap = cached_test_drvs(&mut store, &repo, &base, sys, &base_names)?;
            let hmap = cached_test_drvs(&mut store, &repo, &head, sys, &head_names)?;
            // The same diff the full set went through, so the test rows
            // classify (regression / fixed / new / meta-only …) identically.
            changed.extend(evalfile::changed_tests(&bmap, &hmap));
        }
    }

    // Build both sides of the changed set (skipping anything already known,
    // substitutable, or marked broken) so the report has a real state for every
    // row, not a `❓`.
    if !cli.no_build {
        let targets = assemble_targets(&per_system_changed);
        if !targets.is_empty() {
            build::build_targets(&targets, policy)?;
        }
    }

    // Render from the (now-populated) log: reduce each side to a state.
    let store = store::Store::open(&paths::db_path()?)?;
    let mut per_system = Vec::new();
    for (sys, changed) in &per_system_changed {
        let mut entries = Vec::new();
        for c in changed {
            let base_obs = match &c.base_drv {
                Some(d) => store.load_observations(d)?,
                None => Vec::new(),
            };
            let head_obs = match &c.head_drv {
                Some(d) => store.load_observations(d)?,
                None => Vec::new(),
            };
            entries.push(report::Entry {
                attr: c.attr.clone(),
                base_drv: c.base_drv.clone(),
                head_drv: c.head_drv.clone(),
                base: report::side_state(&c.base_drv, c.base_broken, &base_obs),
                head: report::side_state(&c.head_drv, c.head_broken, &head_obs),
            });
        }
        per_system.push((sys.clone(), entries));
    }
    print!("{}", report::render(&base, &head, &per_system));
    Ok(())
}

fn main() -> Result<()> {
    run(Cli::parse())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ca(
        attr: &str,
        base_drv: Option<&str>,
        head_drv: Option<&str>,
        base_broken: bool,
        head_broken: bool,
    ) -> evalfile::ChangedAttr {
        evalfile::ChangedAttr {
            attr: attr.into(),
            base_drv: base_drv.map(str::to_string),
            head_drv: head_drv.map(str::to_string),
            base_broken,
            head_broken,
        }
    }

    #[test]
    fn assemble_targets_dedups_and_ands_broken() {
        let changed = vec![
            // A meta-only unmarking: same drv both sides, bit flips — the
            // unmarked head side must win (build it).
            ca("unmarked", Some("/d/flip"), Some("/d/flip"), true, false),
            // Aliases sharing one head drv where only some variants are marked
            // (the darwin ollama shape): one unmarked alias ⇒ build.
            ca("tool", Some("/d/t0"), Some("/d/shared"), false, false),
            ca("tool-cuda", Some("/d/t1"), Some("/d/shared"), false, true),
            // Every alias marked ⇒ stays skipped.
            ca("allbroken-a", None, Some("/d/ab"), false, true),
            ca("allbroken-b", None, Some("/d/ab"), false, true),
        ];
        let targets = assemble_targets(&[("sys".into(), changed)]);

        let broken_of = |drv: &str| {
            targets
                .iter()
                .find(|t| t.drv_path == drv)
                .unwrap_or_else(|| panic!("no target for {drv}"))
                .broken
        };
        // Deduped: flip once, shared once, ab once, plus the two base drvs.
        assert_eq!(targets.len(), 5);
        assert!(!broken_of("/d/flip"));
        assert!(!broken_of("/d/shared"));
        assert!(broken_of("/d/ab"));
        assert!(!broken_of("/d/t0"));
        assert!(!broken_of("/d/t1"));
    }

    #[test]
    fn assemble_targets_keeps_systems_apart() {
        // The same drv on two systems is two targets, each with its own bit.
        let a = vec![ca("x", None, Some("/d/x"), false, true)];
        let b = vec![ca("x", None, Some("/d/x"), false, false)];
        let targets = assemble_targets(&[("sysA".into(), a), ("sysB".into(), b)]);
        assert_eq!(targets.len(), 2);
        let of = |sys: &str| targets.iter().find(|t| t.system == sys).unwrap();
        assert!(of("sysA").broken);
        assert!(!of("sysB").broken);
    }
}
