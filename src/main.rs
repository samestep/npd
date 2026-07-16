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

use crate::model::{BuildPolicy, Rev};

#[derive(Parser)]
#[command(
    name = "npd",
    version,
    about = "A persistent fact store for iterating on nixpkgs changes"
)]
struct Cli {
    /// Base-branch tip to review the head against (default: `master`). The
    /// report compares this against the head merged onto it (see `--no-merge`).
    #[arg(long, conflicts_with = "pr")]
    base: Option<String>,
    /// Head revision to review (default: `HEAD`, or the uncommitted working
    /// tree if it has changes).
    #[arg(long, conflicts_with = "pr")]
    head: Option<String>,
    /// Review NixOS/nixpkgs PR #N: shorthand for `--head` = the PR's head,
    /// `--base` = its base-branch tip (GitHub's test-merge commit's first
    /// parent) — the same delta ofborg/Hydra and nixpkgs-review evaluate. The
    /// PR's refs are fetched into the local clone once, so a repeat run
    /// resolves them offline.
    #[arg(long, value_name = "N")]
    pr: Option<u64>,
    /// With --pr: re-fetch the PR's refs even if already cached locally (to
    /// pick up a rebased PR or a base branch that has moved since last run).
    #[arg(long, requires = "pr")]
    refetch: bool,
    /// Diff from the merge-base of base and head, instead of the default —
    /// building a synthetic merge of the head onto the base and diffing the
    /// base against that. The merge-base shape ignores drift on the base since
    /// the fork point; the merge shape reflects the head applied on the current
    /// base (what a merge would actually produce), like a PR's test-merge.
    #[arg(long)]
    no_merge: bool,
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
    /// Skip each changed package's `passthru.tests`. By default npd also
    /// evaluates and builds those tests (on both sides), classifying each
    /// test's `base → head` delta like any other attr — the behaviour ported
    /// from nixpkgs-review's `--tests` (#397).
    #[arg(long)]
    no_tests: bool,
    /// Build the packages npd would otherwise skip — those marked
    /// broken/unsupported/insecure in meta (reported as ⏩ by default, like
    /// nixpkgs-review's "skipped").
    #[arg(long)]
    no_skip: bool,
    /// Everything on: implies --no-skip (tests are on by default).
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

/// Spawn `git -C repo ARGS` and return its completed output — the shared spawn
/// behind [`git`], [`fetch_ref`], and [`resolve_commit`], each of which applies
/// its own exit-code handling to the result.
fn git_output(repo: &std::path::Path, args: &[&str]) -> Result<std::process::Output> {
    Proc::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))
}

/// Run `git -C repo ARGS`; return trimmed stdout, or an error carrying stderr.
fn git(repo: &std::path::Path, args: &[&str]) -> Result<String> {
    let out = git_output(repo, args)?;
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
    let out = git_output(repo, &["fetch", upstream, &refspec])?;
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

/// Resolve the report's `(base, head)` for a PR review. GitHub publishes
/// `refs/pull/N/head` (the PR tip) and, when the PR merges cleanly,
/// `refs/pull/N/merge` — a test-merge commit whose first parent is the
/// base-branch tip and whose second parent is the PR head. So `--pr` is just
/// shorthand for a `(base, head)` pair — base = the base-branch tip
/// (`merge^1`), head = the PR tip (`merge^2`) — onto which the shared
/// merge/`--no-merge` rule (see [`apply_merge`]) then applies, exactly as for a
/// local review. A repeat run reuses the cached refs and touches no network (a
/// rev-parse, not the ~0.2 s merge-base walk).
///
/// The default (merge) shape reuses GitHub's `merge` commit verbatim — it *is*
/// the head merged onto the base — so there's no local merge, and the diff is
/// exactly what ofborg/Hydra evaluate. `--no-merge` diffs from the merge-base of
/// `merge^1` and the PR head (the PR's fork point on its real base branch). A
/// conflicted PR has no `merge` ref: the merge shape then fails with a message
/// pointing at `--no-merge`, and `--no-merge` falls back to the fork point with
/// `master` (the only base we can name without the merge commit).
fn resolve_pr(
    repo: &std::path::Path,
    upstream: &str,
    pr: u64,
    refetch: bool,
    no_merge: bool,
) -> Result<(Rev, Rev)> {
    let head_ref = format!("refs/pull/{pr}/head");
    let merge_ref = format!("refs/pull/{pr}/merge");
    let have_merge = ensure_pr_ref(repo, upstream, &merge_ref, refetch)?;

    if no_merge {
        // Fork-point shape: merge-base(base-branch tip, PR head) → PR head. With
        // the merge commit we know the real base-branch tip (`merge^1`) and the
        // PR head (`merge^2`); without it (a conflicted PR) we fall back to the
        // PR head ref and `master`.
        let (base_tip, head) = if have_merge {
            (
                resolve_commit(repo, &format!("{merge_ref}^1"))?,
                resolve_commit(repo, &format!("{merge_ref}^2"))?,
            )
        } else {
            if !ensure_pr_ref(repo, upstream, &head_ref, refetch)? {
                bail!("PR #{pr} not found on {upstream}");
            }
            (
                resolve_commit(repo, "master")?,
                resolve_commit(repo, &head_ref)?,
            )
        };
        let mb = git_merge_base(repo, &base_tip, &head).context("computing the PR's fork point")?;
        return Ok((commit_source(repo, mb)?, commit_source(repo, head)?));
    }

    if !have_merge {
        // No test-merge commit. Distinguish a conflicted PR from a missing one
        // by whether the (always-published) head ref exists.
        let exists = have_commit(repo, &head_ref) || fetch_ref(repo, upstream, &head_ref)?;
        if exists {
            bail!(
                "PR #{pr} is not mergeable (it conflicts with its base branch), \
                 so GitHub publishes no test-merge commit.\n\
                 Re-run with `--pr {pr} --no-merge` to compare the PR head \
                 against its fork point with master instead."
            );
        }
        bail!("PR #{pr} not found on {upstream}");
    }
    // Merge shape: reuse GitHub's test-merge commit. base = `merge^1` (the real
    // base-branch tip), head = the merge itself; its label is the PR tip
    // (`merge^2`), the commit a human reviews.
    let merge = resolve_commit(repo, &merge_ref)?;
    let base = resolve_commit(repo, &format!("{merge_ref}^1"))?;
    let head_tip = resolve_commit(repo, &format!("{merge_ref}^2"))?;
    Ok((
        commit_source(repo, base)?,
        Rev {
            tree: tree_of(repo, &merge)?,
            commit: merge,
            label: head_tip,
        },
    ))
}

/// `git -C repo ARGS` with extra environment set — the spawn behind the
/// working-tree capture (`worktree_source`), which needs a throwaway
/// `GIT_INDEX_FILE` and pinned author/committer identity+dates. Trimmed stdout,
/// or an error carrying stderr (like [`git`]).
fn git_env(repo: &std::path::Path, envs: &[(&str, &str)], args: &[&str]) -> Result<String> {
    let mut cmd = Proc::new("git");
    cmd.arg("-C").arg(repo).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
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

/// The tree object a commit points at — the eval cache key (see [`Rev`]).
fn tree_of(repo: &std::path::Path, commit: &str) -> Result<String> {
    git(repo, &["rev-parse", &format!("{commit}^{{tree}}")])
}

/// Wrap a resolved commit sha as a [`Rev`]: its tree is the eval cache key, and
/// the sha itself is both `fetchGit`'s commit and the display label.
fn commit_source(repo: &std::path::Path, commit: String) -> Result<Rev> {
    let tree = tree_of(repo, &commit)?;
    Ok(Rev {
        tree,
        label: commit.clone(),
        commit,
    })
}

/// The default head: `HEAD`, or — when the working tree has uncommitted changes
/// — the working tree itself, captured as a synthetic content-addressed commit
/// ([`worktree_source`]). This is what lets `npd` review in-progress work;
/// committing that work as-is later is a cache *hit*, since both resolve to the
/// identical tree (see [`Rev`]).
fn head_source(repo: &std::path::Path) -> Result<Rev> {
    let head = resolve_commit(repo, "HEAD")?;
    match worktree_source(repo, &head)? {
        Some(rev) => {
            eprintln!("head: uncommitted working tree (tree {})", &rev.tree[..12]);
            Ok(rev)
        }
        None => commit_source(repo, head),
    }
}

/// Capture the working tree's uncommitted changes as a [`Rev`], or `None` when
/// there are none. Uses `git stash create`, which snapshots edits/deletions to
/// tracked files and staged-new files — but **not fully-untracked files** (`git
/// add` them to have npd see them) — into a commit *without* disturbing the
/// branch, index, or working tree, and crucially reuses git's real index stat
/// cache, so a clean tree costs ~`git status` time (tens of ms on nixpkgs)
/// rather than re-hashing every tracked file (~1.3 s). Its *tree* is pure
/// content (no timestamp), so an unchanged working tree yields the same eval key
/// on every run (a warm cache hit); over that tree npd mints its *own*
/// deterministic commit for `fetchGit` — pinned identity + epoch dates + parent
/// `head`, so its sha is stable across runs (the stash commit's own sha is not:
/// it embeds the current time, which is exactly why we don't use it) — pinned
/// under `refs/npd/worktree` so a `git gc` can't drop the dangling object before
/// the eval fetches it. Two commits with one tree fetch to the identical source
/// path, so keying on the tree — not this commit — is what makes it cache
/// correctly (DESIGN §6).
fn worktree_source(repo: &std::path::Path, head: &str) -> Result<Option<Rev>> {
    // `stash create` prints the stash commit's sha, or nothing when the tree is
    // clean — either way it leaves the branch/index/working tree untouched.
    let stash = git(repo, &["stash", "create"])?;
    if stash.is_empty() {
        return Ok(None); // nothing uncommitted to review
    }
    let tree = tree_of(repo, &stash)?;
    // A fixed identity + timestamp make the commit sha a pure function of
    // (tree, parent), so the same working tree reproduces it run to run.
    const EPOCH: &str = "1970-01-01T00:00:00Z";
    let ident = [
        ("GIT_AUTHOR_NAME", "npd"),
        ("GIT_AUTHOR_EMAIL", "npd@localhost"),
        ("GIT_AUTHOR_DATE", EPOCH),
        ("GIT_COMMITTER_NAME", "npd"),
        ("GIT_COMMITTER_EMAIL", "npd@localhost"),
        ("GIT_COMMITTER_DATE", EPOCH),
    ];
    let commit = git_env(
        repo,
        &ident,
        &[
            "commit-tree",
            &tree,
            "-p",
            head,
            "-m",
            "npd: uncommitted working tree",
        ],
    )?;
    // Pin so a `git gc` can't drop the dangling commit before fetchGit reads it.
    git(repo, &["update-ref", "refs/npd/worktree", &commit])?;
    Ok(Some(Rev {
        tree,
        commit,
        label: "worktree".into(),
    }))
}

/// Resolve a revision (ref, short/full sha, tag, `HEAD~1`, …) to a full commit
/// sha, so callers can use friendly names even though `fetchGit` needs a rev.
fn resolve_commit(repo: &std::path::Path, rev: &str) -> Result<String> {
    let rev_arg = format!("{rev}^{{commit}}");
    let out = git_output(
        repo,
        &["rev-parse", "--verify", "--quiet", rev_arg.as_str()],
    )?;
    if !out.status.success() {
        bail!("cannot resolve revision {rev:?} in {}", repo.display());
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Resolve the report's `(base, head)` for a local review, with ergonomic
/// defaults: the base-branch tip defaults to `master`, the head to `HEAD` (or
/// the uncommitted working tree, if dirty — see [`head_source`]). An explicit
/// head is always taken literally — the working tree is only ever used on the
/// default path. The shared merge/`--no-merge` rule ([`apply_merge`]) then
/// derives the final pair.
fn resolve_local(
    repo: &std::path::Path,
    base: Option<String>,
    head: Option<String>,
    no_merge: bool,
) -> Result<(Rev, Rev)> {
    let head = match head {
        Some(h) => commit_source(repo, resolve_commit(repo, &h)?)?,
        None => head_source(repo)?,
    };
    let base_tip = match base {
        Some(b) => commit_source(repo, resolve_commit(repo, &b)?)?,
        None => commit_source(repo, resolve_commit(repo, "master")?)
            .context("no base given and no `master` branch to default to; pass --base")?,
    };
    apply_merge(repo, base_tip, head, no_merge)
}

/// Turn a `(base-branch tip, head)` pair into the report's `(base, head)`.
///
/// Default (merge) shape: build a synthetic merge of `head` onto `base` (base
/// as first parent) and report `base → merge`, so the report reflects the head
/// applied on the *current* base — base drift included — exactly what a merge
/// would produce (the same shape a PR's test-merge gives). When the head
/// already descends from the base the merge is a fast-forward, so its tree
/// equals the head's and this collapses to a plain `base → head` at no extra
/// cost; a distinct merged tree (and eval) appears only when the base has
/// genuinely drifted.
///
/// `--no-merge` shape: report `merge-base(base, head) → head`, the fork point —
/// cheap and offline, but blind to base drift since then.
fn apply_merge(
    repo: &std::path::Path,
    base_tip: Rev,
    head: Rev,
    no_merge: bool,
) -> Result<(Rev, Rev)> {
    if no_merge {
        let mb = git_merge_base(repo, &base_tip.commit, &head.commit)
            .context("could not merge-base base and head; pass --base explicitly")?;
        return Ok((commit_source(repo, mb)?, head));
    }
    let merge = merge_source(repo, &base_tip, &head)?;
    Ok((base_tip, merge))
}

/// Mint a deterministic synthetic merge of `head` onto `base` (base as first
/// parent), mirroring [`worktree_source`]: `git merge-tree` produces the merged
/// tree without touching the working tree, and over it we `commit-tree` with a
/// pinned identity + epoch dates so the commit sha is a pure function of
/// `(tree, base, head)` (a repeat run is a cache hit). The commit is pinned
/// under `refs/npd/merge` so `git gc` can't drop it before the eval fetches it.
/// The merge Rev's label is the head's — the report shows `base → the head`,
/// the change under review, with the merge itself implicit. A merge conflict is
/// a hard error pointing at `--no-merge` (a conflicted tree would only miseval).
fn merge_source(repo: &std::path::Path, base: &Rev, head: &Rev) -> Result<Rev> {
    let out = git_output(
        repo,
        &["merge-tree", "--write-tree", &base.commit, &head.commit],
    )?;
    if !out.status.success() {
        bail!(
            "cannot merge {} onto {}: they conflict.\n\
             Re-run with --no-merge to diff from their merge-base instead.",
            &head.label,
            &base.label,
        );
    }
    let tree = String::from_utf8(out.stdout)?.trim().to_string();
    const EPOCH: &str = "1970-01-01T00:00:00Z";
    let ident = [
        ("GIT_AUTHOR_NAME", "npd"),
        ("GIT_AUTHOR_EMAIL", "npd@localhost"),
        ("GIT_AUTHOR_DATE", EPOCH),
        ("GIT_COMMITTER_NAME", "npd"),
        ("GIT_COMMITTER_EMAIL", "npd@localhost"),
        ("GIT_COMMITTER_DATE", EPOCH),
    ];
    let commit = git_env(
        repo,
        &ident,
        &[
            "commit-tree",
            &tree,
            "-p",
            &base.commit,
            "-p",
            &head.commit,
            "-m",
            "npd: synthetic merge",
        ],
    )?;
    git(repo, &["update-ref", "refs/npd/merge", &commit])?;
    Ok(Rev {
        tree,
        commit,
        label: head.label.clone(),
    })
}

/// Flatten the per-system changed sets into build targets: every side's drv,
/// deduped by drv. A drv path is system-specific (the system is part of its
/// input hash), so deduping on the drv alone already keeps systems apart.
///
/// Several `(attr, side)` rows can share one drv with *different* meta-blocked
/// bits — aliases where only some variants are marked (on darwin `ollama-cuda`
/// shares `ollama`'s drv but is marked unsupported), or a meta-only unmarking
/// (a PR deleting `meta.broken` leaves the drv identical on both sides with
/// the bit flipped). The marking is a property of the *attr*, not the recipe,
/// so the deduped target is skipped only if EVERY row wanting this drv is
/// marked: any unmarked row is a legitimate request to build it.
fn assemble_targets(
    per_system_changed: &[(String, Vec<evalfile::ChangedAttr>)],
) -> Vec<build::Target> {
    let mut targets: Vec<build::Target> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (_sys, changed) in per_system_changed {
        for c in changed {
            let sides = [(&c.base_drv, c.base_skipped), (&c.head_drv, c.head_skipped)];
            for (drv, skipped) in sides {
                let Some(drv) = drv else { continue };
                match index.entry(drv.clone()) {
                    Entry::Occupied(e) => {
                        let t = &mut targets[*e.get()];
                        t.skipped = t.skipped && skipped;
                    }
                    Entry::Vacant(e) => {
                        e.insert(targets.len());
                        targets.push(build::Target {
                            drv_path: drv.clone(),
                            skipped,
                        });
                    }
                }
            }
        }
    }
    targets
}

fn run(cli: Cli) -> Result<()> {
    // Tests run by default; --no-tests opts out. --max is "everything on".
    let tests = !cli.no_tests;
    let no_skip = cli.no_skip || cli.max;
    let policy = BuildPolicy {
        recheck: cli.recheck,
        retry: cli.retry,
        prefer_local: cli.prefer_local,
        no_skip,
    };
    let opts = cli.eval;
    let repo = resolve_repo(cli.nixpkgs)?;
    let (base, head) = match cli.pr {
        Some(pr) => resolve_pr(&repo, UPSTREAM, pr, cli.refetch, cli.no_merge)?,
        None => resolve_local(&repo, cli.base, cli.head, cli.no_merge)?,
    };
    let systems = resolve_systems(cli.system);

    eval::eval_two(&repo, &base, &head, &systems, opts)?;

    // The changed set per system — each attr's drv + meta-blocked bit per side —
    // from a linear merge of the two sorted eval files. Computed once, reused
    // for build+render.
    let mut per_system_changed: Vec<(String, Vec<evalfile::ChangedAttr>)> = Vec::new();
    for sys in &systems {
        let changed = evalfile::changed_set(&base.tree, &head.tree, sys)?;
        per_system_changed.push((sys.clone(), changed));
    }

    // --tests: expand each system's changed set with the changed packages'
    // `passthru.tests`. We resolve the tests on *both* sides (through the
    // per-package SQLite cache — see `cached_test_drvs`) and keep a test as a
    // changed attr only when its drv actually differs base→head — exactly
    // `changed_set`'s own semantics, so the test rows classify (regression /
    // fixed / new / …) like every other attr. A side where the package is
    // skipped contributes no tests (a test drv depends on the package,
    // so building it would build the skipped package) unless --no-skip.
    if tests {
        let mut store = store::Store::open(&paths::db_path()?)?;
        // The not-skipped changed-package names per system, on each side.
        let per_sys: Vec<(Vec<String>, Vec<String>)> = per_system_changed
            .iter()
            .map(|(_, changed)| {
                let names_on = |not_skipped: fn(&evalfile::ChangedAttr) -> bool| -> Vec<String> {
                    let mut v: Vec<String> = changed
                        .iter()
                        .filter(|c| no_skip || not_skipped(c))
                        .map(|c| c.attr.clone())
                        .collect();
                    v.sort();
                    v.dedup();
                    v
                };
                (names_on(|c| !c.base_skipped), names_on(|c| !c.head_skipped))
            })
            .collect();

        // Gather the cache misses across every (tree, system, side) and
        // evaluate them all through one scheduler, so `--tests` schedules and
        // displays as a unit like the full eval. Deduped by (tree, system): with
        // `npd X X` (or a base/head sharing a tree) both sides are one key (and
        // per the per-package cache, the same tree on different systems are
        // distinct keys).
        let mut requests: Vec<(Rev, String, Vec<String>)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for ((sys, _), (base_names, head_names)) in per_system_changed.iter().zip(&per_sys) {
            for (rev, names) in [(&base, base_names), (&head, head_names)] {
                if !seen.insert((rev.tree.clone(), sys.clone())) {
                    continue;
                }
                let cached = store.tests_cached_pkgs(&rev.tree, sys, names)?;
                let misses: Vec<String> = names
                    .iter()
                    .filter(|p| !cached.contains(*p))
                    .cloned()
                    .collect();
                if !misses.is_empty() {
                    requests.push((rev.clone(), sys.clone(), misses));
                }
            }
        }
        let jobs_per = eval::eval_tests_many(&repo, &requests)?;
        for ((rev, sys, misses), jobs) in requests.iter().zip(&jobs_per) {
            store.cache_test_eval(&rev.tree, sys, misses, jobs)?;
        }

        // The per-package cache is now populated; build each system's test-row
        // diff. The same diff the full set went through, so test rows classify
        // (regression / fixed / new / meta-only …) identically.
        for ((sys, changed), (base_names, head_names)) in
            per_system_changed.iter_mut().zip(&per_sys)
        {
            let bmap = store.tests_drvs_for(&base.tree, sys, base_names)?;
            let hmap = store.tests_drvs_for(&head.tree, sys, head_names)?;
            changed.extend(evalfile::changed_tests(&bmap, &hmap));
        }
    }

    // Build both sides of the changed set (skipping anything already known,
    // substitutable, or meta-blocked) so the report has a real state for every
    // row, not a `❓`.
    if !cli.no_build {
        let targets = assemble_targets(&per_system_changed);

        // The evals ran with --no-instantiate (no `.drv` writes for the ~114k
        // attrs npd never builds), so materialize the changed set's `.drv`s now —
        // the narinfo probe and the build both read them from the store. But only
        // for drvs the build phase will actually touch: a drv already known
        // built/substitutable/failing is decided from the log alone, so writing
        // its `.drv` is pure waste. When *every* changed drv is already known (the
        // warm-cache iterative loop npd is built for), this set is empty and the
        // whole instantiation eval is skipped — the couple of seconds that
        // otherwise made a fully-cached run non-instant (DESIGN.md §5–§6).
        let need = build::drvs_to_materialize(&targets, policy)?;
        let mut inst: Vec<(Rev, String, Vec<String>)> = Vec::new();
        for (sys, changed) in &per_system_changed {
            let mut base_attrs = Vec::new();
            let mut head_attrs = Vec::new();
            for c in changed {
                let wants = |drv: &Option<String>, skipped: bool| {
                    drv.as_ref()
                        .is_some_and(|d| (no_skip || !skipped) && need.contains(d))
                };
                if wants(&c.base_drv, c.base_skipped) {
                    base_attrs.push(c.attr.clone());
                }
                if wants(&c.head_drv, c.head_skipped) {
                    head_attrs.push(c.attr.clone());
                }
            }
            inst.push((base.clone(), sys.clone(), base_attrs));
            inst.push((head.clone(), sys.clone(), head_attrs));
        }
        eval::instantiate(&repo, &inst)?;

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
                base: report::side_state(&c.base_drv, c.base_skipped, &base_obs),
                head: report::side_state(&c.head_drv, c.head_skipped, &head_obs),
            });
        }
        per_system.push((sys.clone(), entries));
    }
    print!("{}", report::render(&base.label, &head.label, &per_system));
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
        base_skipped: bool,
        head_skipped: bool,
    ) -> evalfile::ChangedAttr {
        evalfile::ChangedAttr {
            attr: attr.into(),
            base_drv: base_drv.map(str::to_string),
            head_drv: head_drv.map(str::to_string),
            base_skipped,
            head_skipped,
        }
    }

    #[test]
    fn assemble_targets_dedups_and_ands_skipped() {
        let changed = vec![
            // A meta-only unmarking: same drv both sides, bit flips — the
            // unmarked head side must win (build it).
            ca("unmarked", Some("/d/flip"), Some("/d/flip"), true, false),
            // Aliases sharing one head drv where only some variants are marked
            // (the darwin ollama shape): one unmarked alias ⇒ build.
            ca("tool", Some("/d/t0"), Some("/d/shared"), false, false),
            ca("tool-cuda", Some("/d/t1"), Some("/d/shared"), false, true),
            // Every alias marked ⇒ stays skipped.
            ca("allskipped-a", None, Some("/d/ab"), false, true),
            ca("allskipped-b", None, Some("/d/ab"), false, true),
        ];
        let targets = assemble_targets(&[("sys".into(), changed)]);

        let skipped_of = |drv: &str| {
            targets
                .iter()
                .find(|t| t.drv_path == drv)
                .unwrap_or_else(|| panic!("no target for {drv}"))
                .skipped
        };
        // Deduped: flip once, shared once, ab once, plus the two base drvs.
        assert_eq!(targets.len(), 5);
        assert!(!skipped_of("/d/flip"));
        assert!(!skipped_of("/d/shared"));
        assert!(skipped_of("/d/ab"));
        assert!(!skipped_of("/d/t0"));
        assert!(!skipped_of("/d/t1"));
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
        // Merge shape (default): reuse GitHub's merge. base = merge^1 (master
        // tip B), head = merge (M), whose label is the PR tip merge^2 (C).
        let (base, head) = resolve_pr(local.path(), upstream, 1, false, false).unwrap();
        assert_eq!(base.commit, s["b"]);
        assert_eq!(head.commit, s["m"]);
        assert_eq!(head.label, s["c"]);
        // --no-merge shape: fork point on the PR's real base branch —
        // merge-base(merge^1 = B, PR tip = C) = A, and head = the PR tip C.
        let (nb, nh) = resolve_pr(local.path(), upstream, 1, false, true).unwrap();
        assert_eq!(nb.commit, s["a"]);
        assert_eq!(nh.commit, s["c"]);
        // Second call is offline (ref now cached) and resolves identically.
        let (base2, head2) = resolve_pr(local.path(), "file:///does/not/exist", 1, false, false)
            .expect("cached ref should resolve without touching upstream");
        assert_eq!(
            (base2.commit, head2.commit),
            (s["b"].clone(), s["m"].clone())
        );
    }

    #[test]
    fn resolve_pr_non_mergeable_errors_and_suggests_no_merge() {
        let (up, s) = pr_fixture();
        let local = tempfile::tempdir().unwrap();
        Proc::new("git")
            .args(["clone", "-q"])
            .arg(up.path())
            .arg(local.path())
            .status()
            .unwrap();
        let upstream = up.path().to_str().unwrap();
        // No merge ref → the merge shape can't apply; a clear error → --no-merge.
        let err = resolve_pr(local.path(), upstream, 2, false, false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not mergeable"), "{msg}");
        assert!(msg.contains("--no-merge"), "{msg}");
        // --no-merge falls back to the fork point with master: head = PR head
        // (D), base = merge-base(master = B, D) = A.
        let (base, head) = resolve_pr(local.path(), upstream, 2, false, true).unwrap();
        assert_eq!(head.commit, s["d"]);
        assert_eq!(base.commit, s["a"]);
    }

    #[test]
    fn merge_source_fast_forwards_when_head_descends_base() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        g(d, &["-c", "init.defaultBranch=master", "init", "."]);
        g(d, &["config", "user.email", "t@t"]);
        g(d, &["config", "user.name", "t"]);
        std::fs::write(d.join("base.txt"), "base\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "A"]);
        let a = commit_source(d, resolve_commit(d, "HEAD").unwrap()).unwrap();
        // A feature commit descending from A.
        std::fs::write(d.join("feat.txt"), "feat\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "F"]);
        let f = commit_source(d, resolve_commit(d, "HEAD").unwrap()).unwrap();

        // F already descends A, so the merge fast-forwards: its tree equals F's
        // (base → merge collapses to A → F, no extra eval). Deterministic sha,
        // pinned for GC-safety, and labelled with the head under review.
        let m = merge_source(d, &a, &f).unwrap();
        let m2 = merge_source(d, &a, &f).unwrap();
        assert_eq!(m.tree, f.tree);
        assert_eq!(m.commit, m2.commit);
        assert_eq!(m.label, f.label);
        assert_eq!(resolve_commit(d, "refs/npd/merge").unwrap(), m.commit);
    }

    #[test]
    fn merge_source_diverges_on_base_drift_and_errors_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        g(d, &["-c", "init.defaultBranch=master", "init", "."]);
        g(d, &["config", "user.email", "t@t"]);
        g(d, &["config", "user.name", "t"]);
        std::fs::write(d.join("shared.txt"), "base\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "A"]);
        let a = commit_source(d, resolve_commit(d, "HEAD").unwrap()).unwrap();

        // Head: add a new file (no overlap with the base's drift).
        g(d, &["checkout", "-b", "head", &a.commit]);
        std::fs::write(d.join("head.txt"), "head\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "H"]);
        let head = commit_source(d, resolve_commit(d, "HEAD").unwrap()).unwrap();

        // Base drifts on a *different* file: a real 3-way merge, whose tree
        // carries both changes and so differs from either side.
        g(d, &["checkout", "-b", "drift", &a.commit]);
        std::fs::write(d.join("drift.txt"), "drift\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "B"]);
        let base = commit_source(d, resolve_commit(d, "HEAD").unwrap()).unwrap();
        let m = merge_source(d, &base, &head).unwrap();
        assert_ne!(m.tree, base.tree);
        assert_ne!(m.tree, head.tree);

        // A base that edits the *same* file the head does conflicts → hard error
        // pointing at --no-merge (a conflicted tree would only miseval).
        g(d, &["checkout", "-b", "clash", &a.commit]);
        std::fs::write(d.join("shared.txt"), "clash\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "C"]);
        let clash = commit_source(d, resolve_commit(d, "HEAD").unwrap()).unwrap();
        // Head must also touch shared.txt to actually conflict.
        g(d, &["checkout", "head"]);
        std::fs::write(d.join("shared.txt"), "head\n").unwrap();
        g(d, &["commit", "-am", "H2"]);
        let head2 = commit_source(d, resolve_commit(d, "HEAD").unwrap()).unwrap();
        let err = merge_source(d, &clash, &head2).unwrap_err();
        assert!(format!("{err}").contains("--no-merge"), "{err}");
    }

    #[test]
    fn worktree_source_is_deterministic_and_tree_keyed() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        g(d, &["-c", "init.defaultBranch=master", "init", "."]);
        g(d, &["config", "user.email", "t@t"]);
        g(d, &["config", "user.name", "t"]);
        std::fs::write(d.join("f.txt"), "one\n").unwrap();
        g(d, &["add", "f.txt"]);
        g(d, &["commit", "-m", "init"]);
        let head = resolve_commit(d, "HEAD").unwrap();

        // A clean tree yields no working-tree source.
        assert!(worktree_source(d, &head).unwrap().is_none());

        // Edit a tracked file: now it's captured.
        std::fs::write(d.join("f.txt"), "two\n").unwrap();
        let a = worktree_source(d, &head)
            .unwrap()
            .expect("dirty tree captured");
        let b = worktree_source(d, &head).unwrap().unwrap();
        // Deterministic across runs: same tree key AND same synthetic commit
        // (the whole point — an unchanged working tree is a warm cache hit).
        assert_eq!(a.tree, b.tree);
        assert_eq!(a.commit, b.commit);
        assert_eq!(a.label, "worktree");
        // The synthetic tree differs from HEAD's, and is pinned for GC-safety.
        assert_ne!(a.tree, tree_of(d, &head).unwrap());
        assert_eq!(resolve_commit(d, "refs/npd/worktree").unwrap(), a.commit);

        // The cache-hit scenario: committing the working tree as-is gives a real
        // commit whose *tree* equals the synthetic one, so the eval key matches
        // and no re-eval is needed.
        g(d, &["commit", "-am", "commit it"]);
        let committed = commit_source(d, resolve_commit(d, "HEAD").unwrap()).unwrap();
        assert_eq!(committed.tree, a.tree);
        assert_ne!(committed.commit, a.commit); // different commit, same tree

        // Tree is clean again; a fully-untracked file is NOT captured (documented
        // limitation — `git stash create` excludes untracked files).
        let now = resolve_commit(d, "HEAD").unwrap();
        assert!(worktree_source(d, &now).unwrap().is_none());
        std::fs::write(d.join("untracked.txt"), "x\n").unwrap();
        assert!(worktree_source(d, &now).unwrap().is_none());
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
    fn assemble_targets_dedups_a_shared_drv_across_systems() {
        // A drv path is system-specific, so a real drv never recurs across
        // systems; but were the same drv to appear on both, it's one recipe and
        // dedups to a single target with the meta-blocked bit AND-merged (any
        // unmarked side is a legitimate request to build it).
        let a = vec![ca("x", None, Some("/d/x"), false, true)];
        let b = vec![ca("x", None, Some("/d/x"), false, false)];
        let targets = assemble_targets(&[("sysA".into(), a), ("sysB".into(), b)]);
        assert_eq!(targets.len(), 1);
        assert!(!targets[0].skipped);
    }
}
