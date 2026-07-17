//! npd — a persistent fact store for iterating on nixpkgs changes.
//!
//! See DESIGN.md for the architecture. The pure data model lives in [`model`];
//! `npd` is a single command that evaluates a `base → head` change, builds
//! whatever the changed set needs, and renders a Markdown report.

mod build;
mod cache;
mod clean;
mod eval;
mod evalfile;
mod live;
mod model;
mod paths;
mod report;
mod store;

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::path::PathBuf;
use std::process::Command as Proc;
use std::sync::{Arc, Mutex};

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
    /// nixpkgs clone to resolve the commits in (default: current directory).
    /// Like git's `-C`, a relative `--patch` file path resolves against this
    /// directory too.
    #[arg(short = 'C')]
    path: Option<PathBuf>,
    /// Base-branch tip to review the head against (default: `master`). The
    /// report compares this against the head merged onto it (see `--no-merge`).
    #[arg(long, conflicts_with = "pr")]
    base: Option<String>,
    /// Head revision to review (default: `HEAD`, or the uncommitted working
    /// tree if it has changes).
    #[arg(long, conflicts_with = "pr")]
    head: Option<String>,
    /// Review a diff applied on top of an anchor, instead of a plain revision —
    /// the head becomes a synthetic content-addressed commit (like the
    /// uncommitted-working-tree capture). The anchor is `--head` if given, else
    /// the default head — the working tree if it has uncommitted changes, else
    /// `HEAD` — so `--patch` composes with work in progress; pass `--head HEAD` to
    /// apply onto the committed tree instead. The value is either a **path** to a
    /// diff file (Nix path syntax: it must contain a `/`, so use `./x.diff`;
    /// resolved against `-C`) or a GitHub **compare expression** `A...B`, whose
    /// endpoints npd resolves locally to shas and fetches as `compare/A...B.diff`.
    /// This is what a report's reproduction command uses to rebuild a PR head
    /// (durably, past the force-pushes PRs rebase through) or an uncommitted
    /// working tree, without needing the original commit fetchable.
    #[arg(long, value_name = "PATH|A...B", conflicts_with = "pr")]
    patch: Option<String>,
    /// Review NixOS/nixpkgs PR #N: shorthand for `--head` = the PR's head,
    /// `--base` = its base-branch tip (GitHub's test-merge commit's first
    /// parent) — the same delta ofborg/Hydra and nixpkgs-review evaluate. The
    /// PR's refs are re-fetched from GitHub every run so the review always
    /// reflects the current PR (a deliberate network fetch, like `--patch A...B`;
    /// hard-errors if GitHub is unreachable, rather than reviewing a stale
    /// snapshot).
    #[arg(long, value_name = "N")]
    pr: Option<u64>,
    /// Diff from the merge-base of base and head, instead of the default —
    /// building a synthetic merge of the head onto the base and diffing the
    /// base against that. The merge-base shape ignores drift on the base since
    /// the fork point; the merge shape reflects the head applied on the current
    /// base (what a merge would actually produce), like a PR's test-merge.
    #[arg(long)]
    no_merge: bool,
    /// Systems to report on (repeatable); defaults to the host system.
    #[arg(short, long)]
    system: Vec<String>,
    /// Re-attempt a previously-failed drv (expect it might pass now).
    #[arg(long)]
    retry: bool,
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
    /// Maintenance: evict cached eval files to bound the cache, then exit
    /// without reviewing (DESIGN.md §4). Takes a size budget (`4GiB`, `500MB` —
    /// keep the most-recently-used evals that fit, drop the least-recently-used
    /// rest), a date (`2026-07-15`), or a duration (`2mo`, `1yr`, `30d` — drop
    /// evals unused for longer). Each evicted eval also purges its `--tests` rows.
    #[arg(
        long,
        value_name = "SIZE|DATE|DURATION",
        conflicts_with_all = ["pr", "base", "head", "patch", "no_merge", "retry", "no_tests", "no_skip"]
    )]
    clean: Option<String>,
    /// Skip `--clean`'s confirmation prompt and delete immediately (for scripts).
    #[arg(short = 'y', long, requires = "clean")]
    yes: bool,
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

/// Fetch `ref_name` from `upstream` into `repo`'s ref of the same name,
/// force-updating it (the `+` refspec) so a moved PR ref is picked up. Returns
/// `Ok(true)` if it now exists, `Ok(false)` if `upstream` has no such ref (a
/// conflicted PR publishes no `merge` ref), and `Err` on any other failure —
/// including an unreachable network, which a `--pr` run treats as fatal rather
/// than silently reviewing a stale snapshot.
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

/// Resolve the report's `(base, head)` for a PR review. GitHub publishes
/// `refs/pull/N/head` (the PR tip) and, when the PR merges cleanly,
/// `refs/pull/N/merge` — a test-merge commit whose first parent is the
/// base-branch tip and whose second parent is the PR head. So `--pr` is just
/// shorthand for a `(base, head)` pair — base = the base-branch tip
/// (`merge^1`), head = the PR tip (`merge^2`) — onto which the shared
/// merge/`--no-merge` rule (see [`apply_merge`]) then applies, exactly as for a
/// local review. Unlike every other path, `--pr` *always* re-fetches the merge
/// ref first — it's a moving pointer GitHub regenerates on a rebase or base
/// move, so a repeat run must reflect the current PR, not a stale snapshot. An
/// unchanged PR makes this a near-free "up to date" fetch, and the tree-keyed
/// eval/build caches still hit (DESIGN §6), so only the pointer is refreshed.
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
    no_merge: bool,
) -> Result<(Rev, Rev)> {
    let head_ref = format!("refs/pull/{pr}/head");
    let merge_ref = format!("refs/pull/{pr}/merge");
    let have_merge = fetch_ref(repo, upstream, &merge_ref)?;

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
            if !fetch_ref(repo, upstream, &head_ref)? {
                bail!("PR #{pr} not found on {upstream}");
            }
            (
                resolve_commit(repo, "master")?,
                resolve_commit(repo, &head_ref)?,
            )
        };
        let mb = git_merge_base(repo, &base_tip, &head).context("computing the PR's fork point")?;
        return Ok((
            commit_source(repo, mb, format!("merge-base(#{pr} base, #{pr} head)"))?,
            commit_source(repo, head, format!("#{pr} head"))?,
        ));
    }

    if !have_merge {
        // No test-merge commit. Distinguish a conflicted PR from a missing one
        // by whether the (always-published) head ref exists.
        let exists = fetch_ref(repo, upstream, &head_ref)?;
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
        commit_source(repo, base, format!("#{pr} base"))?,
        Rev {
            tree: tree_of(repo, &merge)?,
            commit: merge,
            label: head_tip,
            display: format!("#{pr} merge"),
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

/// `git diff --binary A B` in `repo`, returned *untrimmed* — a patch's exact
/// bytes (including any trailing newline) must survive for `git apply` to read
/// it back. Used to reconstruct the diff a live working-tree review captured,
/// for the report's reproduction command.
fn git_diff_binary(repo: &std::path::Path, a: &str, b: &str) -> Result<String> {
    let out = git_output(repo, &["diff", "--binary", a, b])?;
    if !out.status.success() {
        bail!(
            "git diff {a} {b} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// Whether `git diff A B` touches any binary file. `--numstat` prints
/// `added\tdeleted\tpath` per file, with `-\t-` for a binary one — the signal
/// that GitHub's text `.diff` couldn't carry the change, so the reproduction
/// command must embed a `--binary` diff rather than a compare expression.
fn diff_has_binary(repo: &std::path::Path, a: &str, b: &str) -> Result<bool> {
    let out = git(repo, &["diff", "--numstat", a, b])?;
    Ok(out.lines().any(|l| l.starts_with("-\t-\t")))
}

/// The tree object a commit points at — the eval cache key (see [`Rev`]).
fn tree_of(repo: &std::path::Path, commit: &str) -> Result<String> {
    git(repo, &["rev-parse", &format!("{commit}^{{tree}}")])
}

/// Wrap a resolved commit sha as a [`Rev`]: its tree is the eval cache key, the
/// sha is both `fetchGit`'s commit and the [`Rev::label`], and `display` is the
/// human name of this side for the live tree (the ref the user typed, or a
/// default's name — never the resolved sha unless the user typed one).
fn commit_source(repo: &std::path::Path, commit: String, display: String) -> Result<Rev> {
    let tree = tree_of(repo, &commit)?;
    Ok(Rev {
        tree,
        label: commit.clone(),
        commit,
        display,
    })
}

/// The default head: `HEAD`, or — when the working tree has uncommitted changes
/// — the working tree itself, captured as a synthetic content-addressed commit
/// ([`worktree_source`]). This is what lets `npd` review in-progress work;
/// committing that work as-is later is a cache *hit*, since both resolve to the
/// identical tree (see [`Rev`]). Its live-tree `display` is `HEAD` when clean and
/// `HEAD *` when dirty (the same `*` marker the report and `--patch` use).
fn head_source(repo: &std::path::Path) -> Result<Rev> {
    let head = resolve_commit(repo, "HEAD")?;
    match worktree_source(repo, &head)? {
        Some(rev) => Ok(rev),
        None => commit_source(repo, head, "HEAD".into()),
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
    Ok(Some(worktree_commit(
        repo,
        tree_of(repo, &stash)?,
        head,
        "HEAD *".into(),
    )?))
}

/// Mint npd's deterministic synthetic head commit over `tree` with parent
/// `parent`. A fixed identity + epoch dates make the sha a pure function of
/// `(tree, parent)`, so the same content reproduces it run to run, and it's
/// pinned under `refs/npd/worktree` so a `git gc` can't drop the dangling object
/// before `fetchGit` reads it. Shared by the live working-tree capture
/// ([`worktree_source`]) and the `--patch` reconstruction ([`patch_source`]):
/// both are "a synthetic head over an anchor", so both yield the identical Rev
/// for identical content (DESIGN §6).
fn worktree_commit(
    repo: &std::path::Path,
    tree: String,
    parent: &str,
    display: String,
) -> Result<Rev> {
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
            parent,
            "-m",
            "npd: uncommitted working tree",
        ],
    )?;
    git(repo, &["update-ref", "refs/npd/worktree", &commit])?;
    Ok(Rev {
        tree,
        commit,
        label: "worktree".into(),
        display,
    })
}

/// Review a `diff` applied on top of `anchor` (`--patch`): apply it into a
/// throwaway index seeded from `anchor` — never touching the real index or
/// working tree — write that to a tree, and mint the same deterministic
/// synthetic head as the live working-tree capture ([`worktree_commit`]). The
/// tree is pure content, so the eval keys on it regardless of the (ephemeral)
/// commit sha — which is what lets a report's reproduction command rebuild a PR
/// head or a working tree from a diff alone, without the original commit being
/// fetchable (DESIGN §8). The diff must apply cleanly onto `anchor` (npd emits
/// it against exactly that anchor); a failure is fatal rather than a silent
/// mis-review.
fn patch_source(
    repo: &std::path::Path,
    anchor: &str,
    diff: &str,
    anchor_display: &str,
) -> Result<Rev> {
    let anchor = resolve_commit(repo, anchor)?;
    // A throwaway index seeded from the anchor, so `git apply --cached` matches
    // the patch's context against the anchor's blobs without touching anything.
    let dir = tempfile::tempdir().context("creating a temp dir for the --patch index")?;
    let index = dir.path().join("index");
    let patch = dir.path().join("patch.diff");
    std::fs::write(&patch, diff).context("writing the --patch diff to a temp file")?;
    let (index, patch) = (path_str(&index)?, path_str(&patch)?);
    let env = [("GIT_INDEX_FILE", index)];
    git_env(repo, &env, &["read-tree", &anchor])?;
    git_env(repo, &env, &["apply", "--cached", "--binary", patch])
        .context("applying the --patch diff onto the head")?;
    let tree = git_env(repo, &env, &["write-tree"])?;
    // The patched head is `anchor` plus a diff, shown with the same `*` marker
    // the report and the working-tree capture use.
    worktree_commit(repo, tree, &anchor, format!("{anchor_display} *"))
}

/// A path as a UTF-8 `&str`, or an error (paths npd makes are always UTF-8).
fn path_str(p: &std::path::Path) -> Result<&str> {
    p.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", p.display()))
}

/// Fetch a GitHub compare diff (`--patch A...B`) for `NixOS/nixpkgs`. The
/// argument is the `A...B` expression, turned into `compare/A...B.diff`; npd
/// downloads it rather than shelling out to `curl`, so the reproduction command
/// depends on no external binary. A non-2xx or transport error is fatal (like
/// `--pr`, npd won't proceed on a diff it couldn't fetch).
fn fetch_compare_diff(expr: &str) -> Result<String> {
    let url = format!("{UPSTREAM}/compare/{expr}.diff");
    ureq::get(&url)
        .call()
        .with_context(|| format!("fetching {url}"))?
        .into_string()
        .with_context(|| format!("reading the diff from {url}"))
}

/// Pin a compare expression `A...B` to `<shaA>...<shaB>` by resolving *both*
/// endpoints in the local clone (each `resolve_commit`ed exactly once). The
/// resulting immutable expression is what npd hands GitHub — for this review's
/// download *and* for the reproduction command — so re-fetching it later returns
/// the identical diff no matter how `A`/`B` have moved since. The `...`
/// (merge-base) form is preserved: GitHub still diffs `merge-base(shaA, shaB)`
/// against `shaB`, just against fixed commits. Endpoints must therefore resolve
/// locally (and, being shas, exist on GitHub); a name the clone lacks is a hard
/// error here rather than a silently-drifting review later.
fn pin_compare(repo: &std::path::Path, expr: &str) -> Result<String> {
    let (a, b) = expr
        .split_once("...")
        .with_context(|| format!("compare expression must be `A...B`; got {expr:?}"))?;
    let a = resolve_commit(repo, a)?;
    let b = resolve_commit(repo, b)?;
    Ok(format!("{a}...{b}"))
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
    // The human `display` for the head anchor — for `--patch` it names the
    // commit the diff sits on (`HEAD`, a branch), since the `head` arg there is
    // the resolved anchor sha rather than something the user typed.
    head_display: Option<String>,
    no_merge: bool,
    patch: Option<&str>,
) -> Result<(Rev, Rev)> {
    let head = match (head, patch) {
        // --patch: the head is the given diff applied on top of the anchor commit
        // (resolved by the caller — an explicit `--head`, else the default head,
        // which may be the working tree), yielding a synthetic content-addressed
        // commit shown as `<anchor> *`.
        (anchor, Some(diff)) => patch_source(
            repo,
            anchor.as_deref().unwrap_or("HEAD"),
            diff,
            head_display.as_deref().unwrap_or("HEAD"),
        )?,
        // An explicit `--head <ref>` is echoed as the ref the user typed.
        (Some(h), None) => {
            let disp = head_display.unwrap_or_else(|| h.clone());
            commit_source(repo, resolve_commit(repo, &h)?, disp)?
        }
        (None, None) => head_source(repo)?,
    };
    let base_tip = match base {
        Some(b) => commit_source(repo, resolve_commit(repo, &b)?, b)?,
        None => commit_source(repo, resolve_commit(repo, "master")?, "master".into())
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
        // The base side is really the fork point, not the base tip — name it so.
        let display = format!("merge-base({}, {})", base_tip.display, head.display);
        return Ok((commit_source(repo, mb, display)?, head));
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
    // The live-tree name describes the tree actually evaluated: when the merge is
    // a fast-forward its tree equals the head's, so it *is* the head (show it as
    // such); only a genuine drift produces a distinct merge, named `merge(a, b)`.
    let display = if tree == head.tree {
        head.display.clone()
    } else {
        format!("merge({}, {})", base.display, head.display)
    };
    Ok(Rev {
        tree,
        commit,
        label: head.label.clone(),
        display,
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

/// How a report's reproduction command recovers the review's head on another
/// machine (see [`repro_command`]). Which variant applies is decided in [`run`],
/// where the invocation's provenance is known.
enum HeadRepro {
    /// A fetchable commit: `--head <sha>`.
    Commit(String),
    /// Rebuild the head by applying a GitHub compare diff onto `anchor`:
    /// `--head <anchor> --patch <A...B>`. npd downloads `compare/A...B.diff` and
    /// applies it — force-push proof, since GitHub retains commits by sha in its
    /// fork network, so a pinned compare resolves even after a rebase. `expr` is
    /// always sha-pinned (`<shaA>...<shaB>`): `--pr` builds it from the PR's
    /// resolved endpoints, and a `--patch A...B` review pins both endpoints
    /// locally via `pin_compare` — so re-fetching it can never drift.
    Compare { anchor: String, expr: String },
    /// Rebuild the head by applying an embedded `diff` onto `anchor`:
    /// `--head <anchor> --patch /dev/stdin <<'PATCH' … PATCH`. For a diff with no
    /// durable re-fetchable identity — an uncommitted working tree, or a
    /// `--patch <file>` review — so the exact diff rides along in the report and
    /// reproduces byte-for-byte offline.
    Embed { anchor: String, diff: String },
}

/// The shell command a report prints (DESIGN §8) so anyone can reproduce its
/// exact changeset — not the ambiguous invocation the author typed (`npd` alone
/// is a different changeset per machine and day), but the resolved identity.
/// Every form runs `npd --base <sha> --head <…>` on a **pinned base** and a head
/// whose **tree** is pinned: the eval is tree-keyed and the synthetic merge is
/// deterministic (DESIGN §6), so that reproduces the review byte-for-byte. A
/// fetchable head is just `--head <sha>`; otherwise the head is rebuilt with
/// `--patch` (a GitHub compare download, or an embedded diff — see [`HeadRepro`]
/// and the `--patch` flag), so npd does the git plumbing internally and the
/// command calls no external binary. Only flags that change *what the report
/// contains* are echoed (`--no-merge`, `--no-skip`, `--no-tests`, the systems);
/// `--retry` and the eval-sizing knobs don't change the changeset, so they're
/// omitted.
fn repro_command(
    base_sha: &str,
    head: &HeadRepro,
    no_merge: bool,
    no_skip: bool,
    no_tests: bool,
    systems: &[String],
) -> String {
    let mut flags = String::new();
    if no_merge {
        flags.push_str(" --no-merge");
    }
    if no_skip {
        flags.push_str(" --no-skip");
    }
    if no_tests {
        flags.push_str(" --no-tests");
    }
    for s in systems {
        flags.push_str(&format!(" -s {s}"));
    }
    match head {
        HeadRepro::Commit(sha) => format!("npd --base {base_sha} --head {sha}{flags}"),
        HeadRepro::Compare { anchor, expr } => {
            format!("npd --base {base_sha} --head {anchor} --patch {expr}{flags}")
        }
        HeadRepro::Embed { anchor, diff } => {
            let diff = if diff.ends_with('\n') {
                diff.clone()
            } else {
                format!("{diff}\n")
            };
            // A heredoc straight into `--patch /dev/stdin` (just a path npd reads,
            // no `-` special case). `<<'PATCH'` blocks interpolation; a diff body
            // line always has a `+`/`-`/space prefix, so a bare `PATCH` can't occur.
            format!(
                "npd --base {base_sha} --head {anchor} --patch /dev/stdin{flags} <<'PATCH'\n{diff}PATCH"
            )
        }
    }
}

/// The changed set per system: `(system, changed attrs)`, with `--tests` rows
/// folded in. Threaded from [`run_phases`] into the build and the report.
type PerSystemChanged = Vec<(String, Vec<evalfile::ChangedAttr>)>;

/// The not-skipped changed-package names for the `--tests` eval, per side. A
/// package skipped on a side (meta-blocked) contributes no tests there — building
/// a test drv builds the package — unless `--no-skip`. Sorted + deduped.
fn changed_names(changed: &[evalfile::ChangedAttr], no_skip: bool) -> (Vec<String>, Vec<String>) {
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
}

/// Per-system state accumulated as each platform's eval lands (DESIGN §9). Its
/// `Store` lives here rather than being shared by `&` because `rusqlite`'s
/// connection is `!Sync` and this is touched from eval worker threads (behind the
/// mutex in [`run_phases`]).
struct TestsAccum {
    store: store::Store,
    /// Systems whose eval is complete and diff computed (processed once).
    processed: HashSet<String>,
    /// Each system's changed set (pre-`--tests`), assembled in system order later.
    changed: HashMap<String, Vec<evalfile::ChangedAttr>>,
    /// The `tests` phase node, created lazily on the first system with misses.
    tests_phase: Option<Arc<live::Node>>,
    /// The test-listing work + its pre-created (blue) leaves, parallel — executed
    /// grouped after all eval.
    requests: Vec<(Rev, String, Vec<String>)>,
    nodes: Vec<Arc<live::Node>>,
    /// First error from a worker-thread callback, re-raised after eval.
    fatal: Option<anyhow::Error>,
}

/// Reveal a ready system's `tests` leaves (blue) in the tree, in fixed system
/// order, and record its test-listing work for the later grouped execution. A
/// side with no cache misses (or none once the package is skipped) contributes
/// nothing; a system with no misses at all never appears.
#[allow(clippy::too_many_arguments)]
fn reveal_system_tests(
    acc: &mut TestsAccum,
    tree: &live::Tree,
    systems: &[String],
    base: &Rev,
    head: &Rev,
    no_skip: bool,
    sys: &str,
    changed: &[evalfile::ChangedAttr],
) -> Result<()> {
    let (base_names, head_names) = changed_names(changed, no_skip);
    let sys_index = systems.iter().position(|s| s == sys).unwrap() as i64;

    // Sides with cache misses, deduped by tree (a shared base/head tree is one
    // eval key, hence one leaf) — mirroring the old global (tree, system) dedup.
    let mut pending: Vec<(Rev, Vec<String>)> = Vec::new();
    let mut seen_trees = HashSet::new();
    for (rev, names) in [(base, &base_names), (head, &head_names)] {
        if !seen_trees.insert(rev.tree.clone()) {
            continue;
        }
        let cached = acc.store.tests_cached_pkgs(&rev.tree, sys, names)?;
        let misses: Vec<String> = names
            .iter()
            .filter(|p| !cached.contains(*p))
            .cloned()
            .collect();
        if !misses.is_empty() {
            pending.push((rev.clone(), misses));
        }
    }
    if pending.is_empty() {
        return Ok(());
    }

    // Lazily create the `tests` phase node — a plain push lands it after the
    // `evaluate` subtree (instantiate/probe don't exist yet).
    let phase = acc
        .tests_phase
        .get_or_insert_with(|| tree.node("tests", 0))
        .clone();

    // Build this system's subtree (system spine for multi, then its miss leaves)
    // and splice it into fixed system order.
    let depth = if tree.multi() { 2 } else { 1 };
    let mut subtree: Vec<Arc<live::Node>> = Vec::new();
    if tree.multi() {
        subtree.push(tree.detached_node(sys.to_string(), 1, sys_index));
    }
    for (rev, misses) in pending {
        let node = tree.detached_counter(rev.display.clone(), depth, -1, sys_index);
        subtree.push(node.clone());
        acc.requests.push((rev, sys.to_string(), misses));
        acc.nodes.push(node);
    }
    tree.insert_sorted(&phase, subtree);
    Ok(())
}

/// The pre-build phases — everything that runs behind the one live progress tree
/// (DESIGN §6, §9): evaluate both sides, diff to the changed set, expand
/// `--tests`, instantiate the `.drv`s the build will touch, and probe the cache.
/// The nom build and the report come after, outside the tree. Returns the
/// per-system changed set (with test rows folded in) and the build targets.
#[allow(clippy::too_many_arguments)]
fn run_phases(
    repo: &std::path::Path,
    base: &Rev,
    head: &Rev,
    systems: &[String],
    opts: eval::EvalOpts,
    policy: BuildPolicy,
    tests: bool,
    no_skip: bool,
    tree: &live::Tree,
    handle: live::LiveHandle<'_>,
) -> Result<(PerSystemChanged, Vec<build::Target>)> {
    // Per-system state, accumulated as each platform's eval lands so its `tests`
    // appear (blue) the moment its diff is computable — while other platforms are
    // still evaluating — in fixed system order (DESIGN §9). The test-listing
    // itself still runs grouped, after all eval; only the appearance is early.
    let accum = Mutex::new(TestsAccum {
        store: store::Store::open(&paths::db_path()?)?,
        processed: HashSet::new(),
        changed: HashMap::new(),
        tests_phase: None,
        requests: Vec::new(),
        nodes: Vec::new(),
        fatal: None,
    });
    // Process a system once BOTH its eval files exist (cached up front, or cold
    // once evaluated): compute its diff, and — with `--tests` — reveal its test
    // leaves. Called per eval completion (worker threads), for cached systems once
    // the eval nodes exist (main thread, from `eval_pairs`), and a final sweep
    // (main thread). Idempotent via `processed`; the coarse mutex serializes the
    // brief work (a ~12 ms diff, a quick SQLite read, a tree splice), and
    // `rusqlite` being `!Sync` is why the `Store` lives inside it, not shared by `&`.
    let process = |sys: &str| {
        let mut acc = accum.lock().unwrap();
        if acc.fatal.is_some() || acc.processed.contains(sys) {
            return;
        }
        let result = (|| -> Result<()> {
            if !evalfile::eval_path(&base.tree, sys)?.exists()
                || !evalfile::eval_path(&head.tree, sys)?.exists()
            {
                return Ok(()); // not both sides yet
            }
            acc.processed.insert(sys.to_string());
            let changed = evalfile::changed_set(&base.tree, &head.tree, sys)?;
            if tests {
                reveal_system_tests(&mut acc, tree, systems, base, head, no_skip, sys, &changed)?;
            }
            acc.changed.insert(sys.to_string(), changed);
            Ok(())
        })();
        if let Err(e) = result {
            acc.fatal = Some(e);
        }
    };

    // Cold systems fire `process` as their eval lands; systems already cached
    // when eval starts fire once `eval_two` has created the eval nodes (so `tests`
    // still sorts below `evaluate`). The sweep then catches the fully-cached run,
    // where `eval_two` creates no nodes and fires nothing at all.
    eval::eval_two(repo, base, head, systems, opts, tree, handle, &process)?;
    for sys in systems {
        process(sys);
    }

    // Assemble the diffs in system order; surface any callback error.
    let mut acc = accum.lock().unwrap();
    if let Some(e) = acc.fatal.take() {
        return Err(e);
    }
    let mut per_system_changed: PerSystemChanged = systems
        .iter()
        .map(|s| (s.clone(), acc.changed.remove(s).unwrap_or_default()))
        .collect();
    let requests = std::mem::take(&mut acc.requests);
    let nodes = std::mem::take(&mut acc.nodes);
    drop(acc);

    // --tests: run the already-revealed test-listing leaves as one grouped
    // scheduler pass, cache the results, then fold each system's test rows into
    // its changed set — classified (regression / fixed / new / …) like any attr.
    if tests {
        let jobs_per = eval::eval_tests(repo, &requests, nodes, handle)?;
        let mut acc = accum.lock().unwrap();
        for ((rev, sys, misses), jobs) in requests.iter().zip(&jobs_per) {
            acc.store.cache_test_eval(&rev.tree, sys, misses, jobs)?;
        }
        for (sys, changed) in per_system_changed.iter_mut() {
            let (base_names, head_names) = changed_names(changed, no_skip);
            let bmap = acc.store.tests_drvs_for(&base.tree, sys, &base_names)?;
            let hmap = acc.store.tests_drvs_for(&head.tree, sys, &head_names)?;
            changed.extend(evalfile::changed_tests(&bmap, &hmap));
        }
    }

    // Build both sides of the changed set (skipping anything already known,
    // substitutable, or meta-blocked) so the report has a real state for every
    // row.
    let targets = assemble_targets(&per_system_changed);

    // The evals ran with --no-instantiate (no `.drv` writes for the ~114k attrs
    // npd never builds), so materialize the changed set's `.drv`s now — but only
    // for drvs the build phase will actually touch (a drv already known
    // built/substitutable/failing is decided from the log alone). When *every*
    // changed drv is already known (the warm-cache iterative loop), this is empty
    // and the instantiation eval is skipped, keeping the run instant (§5–§6).
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
    eval::instantiate(repo, &inst, tree, handle)?;

    // Probe the cache for the drvs with no fact yet — the run's only network
    // phase inside the tree (a cross-cutting `probe` leaf). Runs before the nom
    // build so the build set is fully decided from facts.
    build::probe_facts(&targets, policy, tree)?;

    Ok((per_system_changed, targets))
}

fn run(cli: Cli) -> Result<()> {
    // `--clean` is a standalone maintenance action (DESIGN.md §4): evict eval
    // files and exit, reviewing nothing. It conflicts with every review knob.
    if let Some(spec) = &cli.clean {
        return clean::clean(&clean::CleanSpec::parse(spec)?, cli.yes);
    }

    // Tests run by default; --no-tests opts out.
    let tests = !cli.no_tests;
    let no_skip = cli.no_skip;
    let policy = BuildPolicy {
        retry: cli.retry,
        no_skip,
    };
    let opts = cli.eval;
    let repo = resolve_repo(cli.path)?;

    let systems = resolve_systems(cli.system);

    // The one persistent progress tree, spanning resolution → probe (the build
    // itself is nom's display). Created BEFORE any network so its clock — and any
    // `fetch`/`download` node — animate live from the first frame: the network can
    // be arbitrarily slow, which is the whole reason to display it. Its number
    // columns start past the widest label the run can produce (DESIGN §6); the
    // base/head `display`s aren't known until resolution finishes, but each phase
    // adds its commit nodes atomically before any shows a count, so the column
    // still never shifts.
    let compare_literal = cli.patch.as_deref().filter(|v| v.contains("..."));
    let tree = live::Tree::new(
        live::plan_label_width(&systems, cli.pr, compare_literal),
        systems.len() > 1,
    );

    // Resolution and every pre-build phase run inside one `with_live`, so the tree
    // is up for the whole slow stretch (the PR / `--patch` fetch included). The
    // closure yields the resolved pair plus the `--patch` bits the reproduction
    // command needs downstream.
    let (base, head, patch_anchor, patch_compare, patch_diff, per_system_changed, targets) =
        live::with_live(
            |tick| tree.render(tick),
            |handle| {
                // --patch: obtain the diff — a local file, or a GitHub compare
                // download (`A...B`). `pin_compare` resolves both endpoints to
                // shas *once* (locally), and that immutable `<shaA>...<shaB>` drives
                // both the download and the reproduction command, so a moved branch
                // can't hand back a different diff on reproduction. The download is
                // a live network node.
                let mut patch_compare: Option<String> = None;
                let patch_diff: Option<String> =
                    match &cli.patch {
                        None => None,
                        Some(value) if value.contains('/') => {
                            // A relative diff path resolves against the `-C` directory
                            // (like git's `-C`), not npd's cwd; a default run's `repo`
                            // is cwd. A local file, so no network node.
                            let p = std::path::Path::new(value);
                            let p = if p.is_absolute() {
                                p.to_path_buf()
                            } else {
                                repo.join(p)
                            };
                            Some(std::fs::read_to_string(&p).with_context(|| {
                                format!("reading the --patch file {}", p.display())
                            })?)
                        }
                        Some(value) if value.contains("...") => {
                            let expr = pin_compare(&repo, value)?;
                            // A live network node, labeled with the literal expression
                            // the user typed — not the sha-pinned form npd fetches.
                            let dl = tree.node("download", 0);
                            let child = tree.node(value.clone(), 1);
                            dl.set_running();
                            child.set_running();
                            let diff = fetch_compare_diff(&expr)?;
                            child.set_done();
                            dl.set_done();
                            patch_compare = Some(expr);
                            Some(diff)
                        }
                        Some(value) => bail!(
                            "--patch must be a path (containing a `/`, e.g. `./x.diff`) or a \
                         compare expression `A...B`; got {value:?}"
                        ),
                    };

                // Resolve the --patch anchor *once* (a mutable ref read twice could
                // move between lookups): an explicit `--head`, else the default head
                // (the working tree if dirty, else HEAD), so `--patch` composes with
                // uncommitted work. A dirty-tree anchor is a synthetic commit the
                // repro embeds.
                let patch_anchor: Option<Rev> = match &cli.patch {
                    Some(_) => Some(match &cli.head {
                        Some(h) => commit_source(&repo, resolve_commit(&repo, h)?, h.clone())?,
                        None => head_source(&repo)?,
                    }),
                    None => None,
                };

                let (base, head) = match cli.pr {
                    Some(pr) => {
                        // The PR ref fetch is a network node — a moving pointer npd
                        // re-fetches every run (DESIGN §6).
                        let f = tree.node("fetch", 0);
                        let m = tree.node(format!("refs/pull/{pr}/merge"), 1);
                        f.set_running();
                        m.set_running();
                        let pair = resolve_pr(&repo, UPSTREAM, pr, cli.no_merge);
                        m.set_done();
                        f.set_done();
                        pair?
                    }
                    None => resolve_local(
                        &repo,
                        cli.base,
                        // For a --patch run the head arg *is* the resolved anchor
                        // commit (patch_source applies the diff onto it); otherwise
                        // it's `--head` verbatim.
                        patch_anchor
                            .as_ref()
                            .map(|r| r.commit.clone())
                            .or_else(|| cli.head.clone()),
                        // …its live-tree name is the anchor's `display` (the typed ref).
                        patch_anchor.as_ref().map(|r| r.display.clone()),
                        cli.no_merge,
                        patch_diff.as_deref(),
                    )?,
                };

                let (per_system_changed, targets) = run_phases(
                    &repo, &base, &head, &systems, opts, policy, tests, no_skip, &tree, handle,
                )?;
                Ok((
                    base,
                    head,
                    patch_anchor,
                    patch_compare,
                    patch_diff,
                    per_system_changed,
                    targets,
                ))
            },
        )?;
    // Freeze the tree as scrollback, then the consistent separator before what
    // follows (nom's build, or the report).
    if !tree.is_empty() {
        for line in tree.render_frozen() {
            eprintln!("{line}");
        }
        live::separator();
    }

    // nom's build runs after the tree freezes; a trailing separator fences its
    // output off from the report (the tree→build separator was printed above).
    if !targets.is_empty() {
        build::build_targets(&targets, policy)?;
        live::separator();
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
    // The head's heading label, and how the reproduction command recovers it
    // elsewhere (DESIGN §8). A head built by applying a diff on top of a commit
    // (a working tree, or `--patch`) is shown as that anchor commit with a
    // trailing `\*` — "this commit, plus a diff" — rather than a bare sha that
    // would read as a plain review of it; a real commit (committed head, or a
    // PR's tip) is shown as-is.
    let (head_display, head_repro) = if cli.pr.is_some() {
        // A PR: rebuild the head from its fork-point diff. npd re-mints the merge
        // from `--base merge^1` and the rebuilt head, so base drift is still
        // shown. The default is a sha-pinned GitHub compare (compact, durable past
        // the force-pushes PRs rebase through). But GitHub's text `.diff` can't
        // carry a binary blob, so a PR that touches binary files falls back to an
        // embedded `git diff --binary` — npd has the PR head locally (`merge^2`),
        // so it computes a binary-capable diff that reproduces offline.
        let fork = git_merge_base(&repo, &base.commit, &head.label)
            .context("computing the PR's fork point for the reproduction command")?;
        let repro = if diff_has_binary(&repo, &fork, &head.label)? {
            let diff = git_diff_binary(&repo, &fork, &head.label)?;
            HeadRepro::Embed { anchor: fork, diff }
        } else {
            HeadRepro::Compare {
                expr: format!("{fork}...{}", head.label),
                anchor: fork,
            }
        };
        (head.label.clone(), repro)
    } else if let Some(anchor) = patch_anchor {
        // Reproducing a --patch run. How depends on the anchor and the diff form:
        if anchor.label == "worktree" {
            // The anchor is the uncommitted working tree — a synthetic, unpushable
            // commit we can't name with `--head`. Embed the whole diff from
            // committed HEAD to the final head tree (worktree + patch), applied
            // onto HEAD, exactly like a bare working-tree review. `patch_source`
            // left `refs/npd/worktree` pointing at that final tree.
            let hsha = resolve_commit(&repo, "HEAD")?;
            let diff = git_diff_binary(&repo, &hsha, "refs/npd/worktree")?;
            (
                format!("{hsha}\\*"),
                HeadRepro::Embed { anchor: hsha, diff },
            )
        } else if let Some(expr) = patch_compare {
            // A compare `--patch A...B` onto a committed anchor: re-emit the
            // sha-pinned compare npd downloaded (`pin_compare` resolved both
            // endpoints once, locally). Immutable, so re-fetching returns the
            // identical diff — no re-resolution, and no embed to bloat the report.
            (
                format!("{}\\*", anchor.commit),
                HeadRepro::Compare {
                    anchor: anchor.commit,
                    expr,
                },
            )
        } else {
            // A file `--patch <path>` onto a committed anchor: the diff is a local
            // file that won't exist elsewhere, so it rides along in the report.
            (
                format!("{}\\*", anchor.commit),
                HeadRepro::Embed {
                    anchor: anchor.commit,
                    diff: patch_diff.unwrap_or_default(),
                },
            )
        }
    } else if head.label == "worktree" {
        // A live uncommitted working tree: embed its captured diff, shown as
        // HEAD with the `\*` diff marker.
        let anchor = resolve_commit(&repo, "HEAD")?;
        let diff = git_diff_binary(&repo, &anchor, "refs/npd/worktree")?;
        (format!("{anchor}\\*"), HeadRepro::Embed { anchor, diff })
    } else {
        (head.label.clone(), HeadRepro::Commit(head.label.clone()))
    };
    let command = repro_command(
        &base.commit,
        &head_repro,
        cli.no_merge,
        no_skip,
        !tests,
        &systems,
    );
    print!(
        "{}",
        report::render(&base.label, &head_display, &command, &per_system)
    );
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
        let (base, head) = resolve_pr(local.path(), upstream, 1, false).unwrap();
        assert_eq!(base.commit, s["b"]);
        assert_eq!(head.commit, s["m"]);
        assert_eq!(head.label, s["c"]);
        // --no-merge shape: fork point on the PR's real base branch —
        // merge-base(merge^1 = B, PR tip = C) = A, and head = the PR tip C.
        let (nb, nh) = resolve_pr(local.path(), upstream, 1, true).unwrap();
        assert_eq!(nb.commit, s["a"]);
        assert_eq!(nh.commit, s["c"]);
        // --pr re-fetches the merge ref every run — even a repeat, and even
        // though it's already cached — so an unreachable upstream is a hard
        // error, never a silent fall back to a possibly-stale local snapshot.
        resolve_pr(local.path(), "file:///does/not/exist", 1, false)
            .expect_err("--pr must re-fetch, so an unreachable upstream errors");
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
        let err = resolve_pr(local.path(), upstream, 2, false).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not mergeable"), "{msg}");
        assert!(msg.contains("--no-merge"), "{msg}");
        // --no-merge falls back to the fork point with master: head = PR head
        // (D), base = merge-base(master = B, D) = A.
        let (base, head) = resolve_pr(local.path(), upstream, 2, true).unwrap();
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
        let a = commit_source(d, resolve_commit(d, "HEAD").unwrap(), "HEAD".into()).unwrap();
        // A feature commit descending from A.
        std::fs::write(d.join("feat.txt"), "feat\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "F"]);
        let f = commit_source(d, resolve_commit(d, "HEAD").unwrap(), "HEAD".into()).unwrap();

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
        let a = commit_source(d, resolve_commit(d, "HEAD").unwrap(), "HEAD".into()).unwrap();

        // Head: add a new file (no overlap with the base's drift).
        g(d, &["checkout", "-b", "head", &a.commit]);
        std::fs::write(d.join("head.txt"), "head\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "H"]);
        let head = commit_source(d, resolve_commit(d, "HEAD").unwrap(), "HEAD".into()).unwrap();

        // Base drifts on a *different* file: a real 3-way merge, whose tree
        // carries both changes and so differs from either side.
        g(d, &["checkout", "-b", "drift", &a.commit]);
        std::fs::write(d.join("drift.txt"), "drift\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "B"]);
        let base = commit_source(d, resolve_commit(d, "HEAD").unwrap(), "HEAD".into()).unwrap();
        let m = merge_source(d, &base, &head).unwrap();
        assert_ne!(m.tree, base.tree);
        assert_ne!(m.tree, head.tree);

        // A base that edits the *same* file the head does conflicts → hard error
        // pointing at --no-merge (a conflicted tree would only miseval).
        g(d, &["checkout", "-b", "clash", &a.commit]);
        std::fs::write(d.join("shared.txt"), "clash\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "C"]);
        let clash = commit_source(d, resolve_commit(d, "HEAD").unwrap(), "HEAD".into()).unwrap();
        // Head must also touch shared.txt to actually conflict.
        g(d, &["checkout", "head"]);
        std::fs::write(d.join("shared.txt"), "head\n").unwrap();
        g(d, &["commit", "-am", "H2"]);
        let head2 = commit_source(d, resolve_commit(d, "HEAD").unwrap(), "HEAD".into()).unwrap();
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
        let committed =
            commit_source(d, resolve_commit(d, "HEAD").unwrap(), "HEAD".into()).unwrap();
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
        let err = resolve_pr(local.path(), up.path().to_str().unwrap(), 99, false).unwrap_err();
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

    #[test]
    fn repro_command_forms() {
        // Committed head: plain --base/--head, only report-shaping flags echoed.
        let cmd = repro_command(
            "aaa",
            &HeadRepro::Commit("bbb".into()),
            false,
            false,
            false,
            &["x86_64-linux".into()],
        );
        assert_eq!(cmd, "npd --base aaa --head bbb -s x86_64-linux");
        let cmd = repro_command(
            "aaa",
            &HeadRepro::Commit("bbb".into()),
            true,
            true,
            true,
            &["a".into(), "b".into()],
        );
        assert_eq!(
            cmd,
            "npd --base aaa --head bbb --no-merge --no-skip --no-tests -s a -s b"
        );

        // Compare (PR): --patch is the compare expression, applied onto --head.
        let cmd = repro_command(
            "m1",
            &HeadRepro::Compare {
                anchor: "fork".into(),
                expr: "fork...m2".into(),
            },
            false,
            false,
            false,
            &["sys".into()],
        );
        assert_eq!(cmd, "npd --base m1 --head fork --patch fork...m2 -s sys");

        // Embed (working tree): a heredoc straight into `--patch /dev/stdin`.
        let cmd = repro_command(
            "b",
            &HeadRepro::Embed {
                anchor: "h".into(),
                diff: "--- a\n+++ b\n".into(),
            },
            false,
            false,
            false,
            &["sys".into()],
        );
        assert_eq!(
            cmd,
            "npd --base b --head h --patch /dev/stdin -s sys <<'PATCH'\n--- a\n+++ b\nPATCH"
        );
    }

    #[test]
    fn patch_source_reconstructs_worktree_tree() {
        // The working-tree reproduction path: capture a dirty tree, take the diff
        // npd would embed, and rebuild it with `patch_source` (what --patch does
        // internally) — the tree must match, from nothing but the diff + HEAD.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        g(d, &["-c", "init.defaultBranch=master", "init", "."]);
        g(d, &["config", "user.email", "t@t"]);
        g(d, &["config", "user.name", "t"]);
        std::fs::write(d.join("f.txt"), "one\n").unwrap();
        std::fs::write(d.join("gone.txt"), "bye\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "init"]);
        let head = resolve_commit(d, "HEAD").unwrap();

        // An edit, a staged-new file, and a deletion — the shapes stash captures.
        std::fs::write(d.join("f.txt"), "two\n").unwrap();
        std::fs::write(d.join("added.txt"), "new\n").unwrap();
        g(d, &["add", "added.txt"]);
        std::fs::remove_file(d.join("gone.txt")).unwrap();
        let wt = worktree_source(d, &head)
            .unwrap()
            .expect("dirty tree captured");
        let diff = git_diff_binary(d, &head, "refs/npd/worktree").unwrap();

        // Pristine tree again; patch_source must rebuild the same tree (and the
        // same deterministic commit worktree_source minted) from the diff alone.
        g(d, &["reset", "--hard", &head]);
        g(d, &["update-ref", "-d", "refs/npd/worktree"]);
        let rebuilt = patch_source(d, &head, &diff, "HEAD").unwrap();
        assert_eq!(rebuilt.tree, wt.tree);
        assert_eq!(rebuilt.commit, wt.commit);
        assert_eq!(rebuilt.label, "worktree");
    }

    #[test]
    fn patch_source_reconstructs_pr_head_and_merge() {
        // A PR shape: a fork point, base drift on a *different* file (a genuine
        // 3-way merge), and a two-commit PR head. Applying the fork→tip compare
        // diff (what GitHub serves) via patch_source must rebuild the tip's tree
        // and reproduce the same test-merge onto merge^1 — offline.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        g(d, &["-c", "init.defaultBranch=master", "init", "."]);
        g(d, &["config", "user.email", "t@t"]);
        g(d, &["config", "user.name", "t"]);
        std::fs::write(d.join("pkg.txt"), "v1\n").unwrap();
        std::fs::write(d.join("other.txt"), "base\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "fork"]);
        let fork = resolve_commit(d, "HEAD").unwrap();
        std::fs::write(d.join("other.txt"), "base2\n").unwrap();
        g(d, &["commit", "-am", "drift"]);
        let m1 = resolve_commit(d, "HEAD").unwrap();
        g(d, &["checkout", "-q", &fork]);
        std::fs::write(d.join("pkg.txt"), "v2\n").unwrap();
        g(d, &["commit", "-am", "pr1"]);
        std::fs::write(d.join("pkg.txt"), "v3\n").unwrap();
        g(d, &["commit", "-am", "pr2"]);
        let m2 = resolve_commit(d, "HEAD").unwrap();

        // The diff GitHub's compare/fork...m2.diff serves, applied onto the fork.
        let diff = git_diff_binary(d, &fork, &m2).unwrap();
        g(d, &["checkout", "-q", &m1]);
        let rebuilt = patch_source(d, &fork, &diff, "HEAD").unwrap();
        assert_eq!(
            tree_of(d, &rebuilt.commit).unwrap(),
            tree_of(d, &m2).unwrap()
        );
        let real_merge = g(d, &["merge-tree", "--write-tree", &m1, &m2]);
        let repro_merge = g(d, &["merge-tree", "--write-tree", &m1, &rebuilt.commit]);
        assert_eq!(real_merge, repro_merge);
    }

    #[test]
    fn pin_compare_pins_both_endpoints_locally() {
        // A compare `A...B` must be pinned to `<shaA>...<shaB>` against the local
        // clone, so the expression handed to GitHub — for the download *and* the
        // repro — is immutable and can't drift when `A`/`B` later move.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        g(d, &["-c", "init.defaultBranch=master", "init", "."]);
        g(d, &["config", "user.email", "t@t"]);
        g(d, &["config", "user.name", "t"]);
        std::fs::write(d.join("f.txt"), "one\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "a"]);
        let a = resolve_commit(d, "HEAD").unwrap();
        std::fs::write(d.join("f.txt"), "two\n").unwrap();
        g(d, &["commit", "-am", "b"]);
        let b = resolve_commit(d, "HEAD").unwrap();

        // A branch endpoint pins to the sha it currently names; a sha endpoint
        // passes through (resolving a full sha is idempotent).
        assert_eq!(
            pin_compare(d, &format!("{a}...master")).unwrap(),
            format!("{a}...{b}")
        );
        assert_eq!(
            pin_compare(d, &format!("{a}...{b}")).unwrap(),
            format!("{a}...{b}")
        );
        // A malformed expression and an unresolvable endpoint are hard errors,
        // not a silently-mispinned compare.
        assert!(pin_compare(d, "only-one-side").is_err());
        assert!(pin_compare(d, &format!("{a}...no-such-ref")).is_err());
    }

    #[test]
    fn diff_has_binary_flags_only_binary_changes() {
        // A binary change (which GitHub's text `.diff` can't carry) must be
        // detected so the PR repro embeds a `--binary` diff instead of a compare;
        // a text-only change must not trip it.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        g(d, &["-c", "init.defaultBranch=master", "init", "."]);
        g(d, &["config", "user.email", "t@t"]);
        g(d, &["config", "user.name", "t"]);
        std::fs::write(d.join("f.txt"), "one\n").unwrap();
        g(d, &["add", "."]);
        g(d, &["commit", "-m", "base"]);
        let base = resolve_commit(d, "HEAD").unwrap();

        // A text-only change: not binary.
        std::fs::write(d.join("f.txt"), "two\n").unwrap();
        g(d, &["commit", "-am", "text"]);
        let text = resolve_commit(d, "HEAD").unwrap();
        assert!(!diff_has_binary(d, &base, &text).unwrap());

        // Add a NUL-containing file: git treats it as binary.
        std::fs::write(d.join("blob.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        g(d, &["add", "blob.bin"]);
        g(d, &["commit", "-m", "binary"]);
        let bin = resolve_commit(d, "HEAD").unwrap();
        assert!(diff_has_binary(d, &text, &bin).unwrap());
        // The span still counts as binary when text changes are mixed in.
        assert!(diff_has_binary(d, &base, &bin).unwrap());
    }
}
