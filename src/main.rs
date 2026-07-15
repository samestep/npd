//! npd — a persistent fact store for iterating on nixpkgs changes.
//!
//! See DESIGN.md for the architecture. The pure data model lives in [`model`];
//! `npd` is a single command that evaluates a `base → head` change, builds
//! whatever the changed set needs, and renders a Markdown report.

mod build;
mod cache;
mod eval;
mod evalfile;
mod live;
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
    #[arg(conflicts_with = "pr")]
    base: Option<String>,
    /// Head revision (default: `HEAD`).
    #[arg(conflicts_with = "pr")]
    head: Option<String>,
    /// Review NixOS/nixpkgs PR #N: compare its base branch against the PR
    /// merged into that branch (GitHub's test-merge commit) — the same delta
    /// ofborg/Hydra and nixpkgs-review evaluate. The PR's refs are fetched
    /// into the local clone once, so a repeat run resolves them offline.
    #[arg(long, value_name = "N")]
    pr: Option<u64>,
    /// With --pr: re-fetch the PR's refs even if already cached locally (to
    /// pick up a rebased PR or a base branch that has moved since last run).
    #[arg(long, requires = "pr")]
    refetch: bool,
    /// With --pr on a non-mergeable PR (no test-merge commit): compare the PR
    /// head against its fork point with `master` instead.
    #[arg(long, requires = "pr")]
    fork_point: bool,
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

/// The canonical repo to fetch PR refs from: a PR number is a NixOS/nixpkgs
/// identity, and the local clone's `origin` may well be a personal fork, so we
/// never rely on a configured remote.
const UPSTREAM: &str = "https://github.com/NixOS/nixpkgs";

/// Run `git -C repo ARGS`; return trimmed stdout, or an error carrying stderr.
fn git(repo: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = Proc::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// `git merge-base base head` in `repo`.
fn git_merge_base(repo: &std::path::Path, base: &str, head: &str) -> Result<String> {
    git(repo, &["merge-base", base, head])
}

/// Is `rev` resolvable to a commit in `repo`? (No network.)
fn have_commit(repo: &std::path::Path, rev: &str) -> bool {
    resolve_commit(repo, rev).is_ok()
}

/// Fetch `ref_name` from `upstream` into `repo`'s ref of the same name. Returns
/// `Ok(true)` if it now exists, `Ok(false)` if `upstream` has no such ref (a
/// conflicted PR publishes no `merge` ref), and `Err` on any other failure.
fn fetch_ref(repo: &std::path::Path, upstream: &str, ref_name: &str) -> Result<bool> {
    let refspec = format!("+{ref_name}:{ref_name}");
    let out = Proc::new("git")
        .arg("-C")
        .arg(repo)
        .args(["fetch", upstream, &refspec])
        .output()
        .context("running git fetch")?;
    if out.status.success() {
        return Ok(true);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    if err.contains("couldn't find remote ref") {
        return Ok(false);
    }
    bail!(
        "git fetch {ref_name} from {upstream} failed: {}",
        err.trim()
    );
}

/// Ensure PR #`pr`'s `ref_name` is present in `repo`, fetching from `upstream`
/// when it is absent (or always, when `refetch`). Returns whether it exists.
fn ensure_pr_ref(
    repo: &std::path::Path,
    upstream: &str,
    ref_name: &str,
    refetch: bool,
) -> Result<bool> {
    if !refetch && have_commit(repo, ref_name) {
        return Ok(true);
    }
    fetch_ref(repo, upstream, ref_name)
}

/// Resolve `(base, head)` for a PR review. GitHub publishes `refs/pull/N/head`
/// (the PR tip) and, when the PR merges cleanly, `refs/pull/N/merge` — a
/// test-merge commit whose first parent is the base-branch tip and whose second
/// parent is the PR head. So `merge^1 → merge` is exactly the PR's patch applied
/// on the *current* base branch (whatever branch that is), which sidesteps both
/// merge-base downsides at once: the correct base branch, up to date. A repeat
/// run reuses the cached refs and touches no network (a rev-parse, not the
/// ~0.2 s merge-base walk).
///
/// `--fork-point` opts back into the old shape (PR head vs its merge-base with
/// `master`) for the one case the merge commit can't cover: a PR that doesn't
/// merge cleanly, so GitHub publishes no `merge` ref.
fn resolve_pr(
    repo: &std::path::Path,
    upstream: &str,
    pr: u64,
    refetch: bool,
    fork_point: bool,
) -> Result<(String, String)> {
    let head_ref = format!("refs/pull/{pr}/head");
    if fork_point {
        if !ensure_pr_ref(repo, upstream, &head_ref, refetch)? {
            bail!("PR #{pr} not found on {upstream}");
        }
        let head = resolve_commit(repo, &head_ref)?;
        let base = git_merge_base(repo, "master", &head)
            .context("computing the PR's fork point with master")?;
        return Ok((base, head));
    }

    let merge_ref = format!("refs/pull/{pr}/merge");
    if !ensure_pr_ref(repo, upstream, &merge_ref, refetch)? {
        // No test-merge commit. Distinguish a conflicted PR from a missing one
        // by whether the (always-published) head ref exists.
        let exists = have_commit(repo, &head_ref) || fetch_ref(repo, upstream, &head_ref)?;
        if exists {
            bail!(
                "PR #{pr} is not mergeable (it conflicts with its base branch), \
                 so GitHub publishes no test-merge commit.\n\
                 Re-run with `--pr {pr} --fork-point` to compare the PR head \
                 against its fork point with master instead."
            );
        }
        bail!("PR #{pr} not found on {upstream}");
    }
    let head = resolve_commit(repo, &merge_ref)?;
    let base = resolve_commit(repo, &format!("{merge_ref}^1"))?;
    Ok((base, head))
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
    let (base, head) = match cli.pr {
        Some(pr) => resolve_pr(&repo, UPSTREAM, pr, cli.refetch, cli.fork_point)?,
        None => resolve_base_head(&repo, cli.base, cli.head)?,
    };
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
        // The unbroken changed-package names per system, on each side.
        let per_sys: Vec<(Vec<String>, Vec<String>)> = per_system_changed
            .iter()
            .map(|(_, changed)| {
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
                (names_on(|c| !c.base_broken), names_on(|c| !c.head_broken))
            })
            .collect();

        // Gather the cache misses across every (commit, system, side) and
        // evaluate them all through one scheduler, so `--tests` schedules and
        // displays as a unit like the full eval. Deduped by (commit, system):
        // with `npd X X`, base and head are the same key (and per the per-package
        // cache, the same commit on different systems are distinct keys).
        let mut requests: Vec<(String, String, Vec<String>)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for ((sys, _), (base_names, head_names)) in per_system_changed.iter().zip(&per_sys) {
            for (commit, names) in [(&base, base_names), (&head, head_names)] {
                if !seen.insert((commit.clone(), sys.clone())) {
                    continue;
                }
                let cached = store.tests_cached_pkgs(commit, sys, names)?;
                let misses: Vec<String> = names
                    .iter()
                    .filter(|p| !cached.contains(*p))
                    .cloned()
                    .collect();
                if !misses.is_empty() {
                    requests.push((commit.clone(), sys.clone(), misses));
                }
            }
        }
        let jobs_per = eval::eval_tests_many(&repo, &requests)?;
        for ((commit, sys, misses), jobs) in requests.iter().zip(&jobs_per) {
            store.cache_test_eval(commit, sys, misses, jobs)?;
        }

        // The per-package cache is now populated; build each system's test-row
        // diff. The same diff the full set went through, so test rows classify
        // (regression / fixed / new / meta-only …) identically.
        for ((sys, changed), (base_names, head_names)) in
            per_system_changed.iter_mut().zip(&per_sys)
        {
            let bmap = store.tests_drvs_for(&base, sys, base_names)?;
            let hmap = store.tests_drvs_for(&head, sys, head_names)?;
            changed.extend(evalfile::changed_tests(&bmap, &hmap));
        }
    }

    // Build both sides of the changed set (skipping anything already known,
    // substitutable, or marked broken) so the report has a real state for every
    // row, not a `❓`.
    if !cli.no_build {
        // The evals ran with --no-instantiate (no `.drv` writes for the ~114k
        // attrs npd never builds), so materialize just the changed set's `.drv`s
        // now — the narinfo probe and the build both read them from the store.
        let mut inst: Vec<(String, String, Vec<String>)> = Vec::new();
        for (sys, changed) in &per_system_changed {
            let mut base_attrs = Vec::new();
            let mut head_attrs = Vec::new();
            for c in changed {
                if c.base_drv.is_some() && (build_broken || !c.base_broken) {
                    base_attrs.push(c.attr.clone());
                }
                if c.head_drv.is_some() && (build_broken || !c.head_broken) {
                    head_attrs.push(c.attr.clone());
                }
            }
            inst.push((base.clone(), sys.clone(), base_attrs));
            inst.push((head.clone(), sys.clone(), head_attrs));
        }
        eval::instantiate(&repo, &inst)?;

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

    /// Run git in `dir`, returning trimmed stdout; panics on failure.
    fn g(dir: &std::path::Path, args: &[&str]) -> String {
        let out = Proc::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Build an "upstream" repo with a mergeable PR #1 (a real `merge` ref whose
    /// first parent is the master tip) and a non-mergeable PR #2 (a `head` ref
    /// only). Returns (upstream_dir, a-map-of-labels→sha).
    fn pr_fixture() -> (tempfile::TempDir, HashMap<&'static str, String>) {
        let up = tempfile::tempdir().unwrap();
        let d = up.path();
        g(d, &["-c", "init.defaultBranch=master", "init", "."]);
        g(d, &["config", "user.email", "t@t"]);
        g(d, &["config", "user.name", "t"]);
        g(d, &["commit", "--allow-empty", "-m", "A"]);
        let a = g(d, &["rev-parse", "HEAD"]);
        g(d, &["commit", "--allow-empty", "-m", "B"]);
        let b = g(d, &["rev-parse", "HEAD"]); // master tip
        // PR #1: a clean feature branch off A, merged back with --no-ff.
        g(d, &["checkout", "-b", "pr1", &a]);
        g(d, &["commit", "--allow-empty", "-m", "C"]);
        let c = g(d, &["rev-parse", "HEAD"]);
        g(d, &["checkout", "master"]);
        g(d, &["merge", "--no-ff", "-m", "M", "pr1"]);
        let m = g(d, &["rev-parse", "HEAD"]);
        g(d, &["update-ref", "refs/pull/1/head", &c]);
        g(d, &["update-ref", "refs/pull/1/merge", &m]);
        // PR #2: a head ref with no merge ref (models a conflicted PR).
        g(d, &["checkout", "-b", "pr2", &a]);
        g(d, &["commit", "--allow-empty", "-m", "D"]);
        let dsha = g(d, &["rev-parse", "HEAD"]);
        g(d, &["update-ref", "refs/pull/2/head", &dsha]);
        g(d, &["checkout", "master"]);
        let shas = HashMap::from([("a", a), ("b", b), ("c", c), ("m", m), ("d", dsha)]);
        (up, shas)
    }

    #[test]
    fn resolve_pr_mergeable_uses_merge_and_its_first_parent() {
        let (up, s) = pr_fixture();
        let local = tempfile::tempdir().unwrap();
        assert!(
            Proc::new("git")
                .args(["clone", "-q"])
                .arg(up.path())
                .arg(local.path())
                .status()
                .unwrap()
                .success()
        );
        let upstream = up.path().to_str().unwrap();
        // Fetches the merge ref; base = merge^1 (master tip B), head = merge (M).
        let (base, head) = resolve_pr(local.path(), upstream, 1, false, false).unwrap();
        assert_eq!(base, s["b"]);
        assert_eq!(head, s["m"]);
        // Second call is offline (ref now cached) and resolves identically.
        let (base2, head2) = resolve_pr(local.path(), "file:///does/not/exist", 1, false, false)
            .expect("cached ref should resolve without touching upstream");
        assert_eq!((base2, head2), (s["b"].clone(), s["m"].clone()));
    }

    #[test]
    fn resolve_pr_non_mergeable_errors_and_suggests_fork_point() {
        let (up, s) = pr_fixture();
        let local = tempfile::tempdir().unwrap();
        Proc::new("git")
            .args(["clone", "-q"])
            .arg(up.path())
            .arg(local.path())
            .status()
            .unwrap();
        let upstream = up.path().to_str().unwrap();
        // No merge ref → a clear error pointing at --fork-point.
        let err = resolve_pr(local.path(), upstream, 2, false, false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not mergeable"), "{msg}");
        assert!(msg.contains("--fork-point"), "{msg}");
        // --fork-point: head = PR head (D), base = merge-base(master, D) = A.
        let (base, head) = resolve_pr(local.path(), upstream, 2, false, true).unwrap();
        assert_eq!(head, s["d"]);
        assert_eq!(base, s["a"]);
    }

    #[test]
    fn resolve_pr_missing_pr_errors() {
        let (up, _s) = pr_fixture();
        let local = tempfile::tempdir().unwrap();
        Proc::new("git")
            .args(["clone", "-q"])
            .arg(up.path())
            .arg(local.path())
            .status()
            .unwrap();
        let err =
            resolve_pr(local.path(), up.path().to_str().unwrap(), 99, false, false).unwrap_err();
        assert!(format!("{err}").contains("not found"), "{err}");
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
