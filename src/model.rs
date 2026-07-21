//! npd core data model.
//!
//! Pure data types the rest of npd is organized around. The guiding decision
//! (see DESIGN.md §2): build facts are keyed on the *derivation path* — the
//! stable identity of a build recipe. It survives failures (unlike an output
//! path) and is shared across commits automatically.
//!
//! Nothing here performs I/O. That keeps the model deterministic and trivially
//! testable and lets the orchestration layer own all the impurity.

/// A revision to evaluate, split into the two git identities it plays plus a
/// display label (DESIGN.md §6).
///
/// The eval is a pure function of the source **tree** — the checked-out file
/// content — not of the commit that carries it. A commit adds parents, an
/// author, a message, and timestamps, none of which the evaluation can observe:
/// `fetchGit`'s checkout has no `.git`, and npd forwards only the resulting
/// *path* into `import`. So the eval (and `--tests`) cache keys on [`tree`]: two
/// commits with the same tree share one eval — a rebase that doesn't touch the
/// changed files, a message-only `--amend`, a cherry-pick landing identical
/// content, and, crucially, committing an as-is working tree (so an
/// uncommitted-then-committed edit is a cache *hit*).
///
/// [`commit`] is a commit that realizes that tree, for `builtins.fetchGit`
/// (which fetches by commit, not by a bare tree). For a committed state it is
/// the real commit; for the uncommitted working tree it is a synthetic,
/// content-addressed commit minted over the tree. [`label`] identifies the
/// side: the commit sha for a real revision, or `worktree` for a synthetic
/// working-tree/patch head — the report renders the latter as its anchor commit
/// with a trailing `\*` ("this commit, plus a diff"), not the bare word.
///
/// [`display`] is the *human* name of the side for the live progress tree
/// ([`crate::live`]): the ref the user actually expressed (or the default's
/// name) rather than a resolved sha — `master`, `HEAD`, a branch, `#431 base` —
/// and, for a commit npd *derives*, an honest description of it: `merge(a, b)`
/// for a synthetic merge, `merge-base(a, b)` for a `--no-merge` fork point,
/// `HEAD*` for a working-tree/patch head. It describes the tree actually
/// evaluated (DESIGN §6), so a sha appears only if the user typed one. Distinct
/// from [`label`] precisely because `label` is a real committish the repro path
/// feeds to `git`, and the report heading keeps showing it as a sha (GitHub
/// auto-links it); the tree wants the friendly form.
///
/// [`tree`]: Rev::tree
/// [`commit`]: Rev::commit
/// [`label`]: Rev::label
/// [`display`]: Rev::display
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rev {
    /// The git tree hash — the eval cache key.
    pub tree: String,
    /// A commit realizing `tree`, used as `builtins.fetchGit`'s `rev`.
    pub commit: String,
    /// Identity label: a commit sha, or `worktree` for a synthetic
    /// working-tree/patch head (rendered as its anchor commit + `\*`).
    pub label: String,
    /// Human name of this side for the live progress tree (see type docs).
    pub display: String,
}

/// Result of evaluating one attribute on one platform at one commit.
///
/// Pure fact: fully determined by (tree, system, config). `drv_path` is
/// `None` when evaluation itself errored (assertion, IFD failure, …) — distinct
/// from a *build* failure, which is an [`Observation`]. The diff and report
/// deliberately render an errored attr as *absent* (➖): in a delta view an
/// eval breakage is visible as the attr disappearing, so no separate error
/// state is needed. `skipped` folds
/// `meta.broken` / `meta.unsupported` / `meta.insecure` into one bit — npd's
/// analogue of nixpkgs-review's "skipped" (its meta-blocked subset; a *missing*
/// attr is a separate state, ➖ absent): the profile's allow-flags let such a
/// package evaluate to a drv anyway, but by default it is not *built* (like
/// nixpkgs-review) — see [`BuildPolicy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrEval {
    pub attr: String,
    pub drv_path: Option<String>,
    pub skipped: bool,
}

/// One resolved `passthru.tests` entry from a targeted test eval (`--tests`).
///
/// Pure fact like [`AttrEval`], but decomposed for the per-package test cache:
/// `pkg_attr` is the package the test hangs off (the attr-path's first element),
/// `test_attr` is the full `<pkg>.tests.<name>` label, and `drv_path` is `None`
/// when the test errored (no derivation) — the same shape the full-set walk gives
/// an errored attr. `skipped` is the test's own meta-blocked bit (broken /
/// unsupported-on-this-system / insecure) — a test can be unavailable even when
/// its package is fine (e.g. an x86-only NixOS test hung off a cross-platform
/// package on `aarch64-linux`), so it must be tracked per test, not inferred from
/// the package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestJob {
    pub pkg_attr: String,
    pub test_attr: String,
    pub drv_path: Option<String>,
    pub skipped: bool,
}

/// The result of a single build attempt/observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The drv's outputs were observed valid: a local build succeeded, or a
    /// substituter has them (a narinfo probe hit, DESIGN.md §7). The log
    /// deliberately doesn't record which — a success is a success.
    Built,
    /// The derivation itself failed to build.
    Failed,
    /// A (transitive) dependency failed; this drv never ran.
    DepFailed,
}

/// One append-only fact about one derivation, keyed externally by `drv_path`.
///
/// We never overwrite an observation; flakiness is simply multiple observations
/// of the same `drv_path` with differing outcomes. Rows carry no timestamp —
/// the log is append-only, so insertion order *is* the history.
///
/// `blocker` holds the output paths whose store validity re-decides this fact
/// (DESIGN.md §5), populated for the two failure outcomes: for a
/// [`Outcome::DepFailed`] it is the *specific* still-failing dependency that
/// blocked this drv (the "culprit"); for an [`Outcome::Failed`] it is the drv's
/// *own* outputs. Either way it makes the failure *self-healing* without
/// re-evaluation: a later run re-checks those paths offline — no `.drv`, no
/// closure walk — and the moment they are valid (the culprit built/substituted,
/// or the drv itself built out of band) the stale failure is overridden. Empty
/// for a `Built`, and for a failure whose paths weren't recorded
/// (treated conservatively as still-failing).
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub drv_path: String,
    pub outcome: Outcome,
    /// Paths whose validity re-decides this fact: a `DepFailed`'s culprit
    /// outputs, or a `Failed`'s own outputs; else empty.
    pub blocker: Vec<String>,
}

/// What the build policy says to do about a derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Never observed, or explicitly forced — attempt it.
    Build,
    /// A trusted success exists — don't rebuild.
    SkipOk,
    /// Only failures observed — don't waste time (unless `retry`).
    SkipFail,
    /// Meta-blocked (broken/unsupported/insecure) and `no_skip` is off — not
    /// attempted (like nixpkgs-review); the report shows it as ⏩ (nixpkgs-review's
    /// "skipped").
    Skipped,
}

/// Turns a derivation's observation history into an action.
///
/// The ergonomic core (DESIGN.md §5): the cache-bypass knobs are just fields,
/// and [`BuildPolicy::decide`] is a pure predicate over the append-only log
/// plus whether the output is substitutable.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildPolicy {
    /// Re-attempt a previously-failed drv (expect it might pass now).
    pub retry: bool,
    /// Build the packages npd would otherwise skip for being meta-blocked
    /// (broken/unsupported/insecure) — off by default, like nixpkgs-review.
    pub no_skip: bool,
}

impl BuildPolicy {
    /// Decide whether to build `drv_path` given its observations.
    ///
    /// `skipped` is the attr's meta-blocked (broken/unsupported/insecure) bit
    /// from the eval. Substituter presence needs no input of its own: a cache
    /// probe's hit is recorded as a plain `Built` observation (DESIGN.md §7),
    /// so it decides here exactly like any other success.
    ///
    /// `dep_block_stale` distinguishes the two kinds of recorded failure
    /// (DESIGN.md §5). A **direct** failure (the drv's own build failed) is
    /// sticky: presumed to keep failing, `--retry` to re-attempt. A
    /// **dependency block** (`DepFailed`) is only trusted while the blocking
    /// dependency is *still* failing; once that culprit has built or been
    /// substituted, the block is stale and the caller passes
    /// `dep_block_stale = true` so we re-attempt — no `--retry` needed. The
    /// caller computes staleness by re-checking the culprit's store validity
    /// (`Observation::blocker`), which the pure predicate can't do itself.
    pub fn decide(
        &self,
        observations: &[Observation],
        skipped: bool,
        dep_block_stale: bool,
    ) -> Decision {
        let built = observations.iter().any(|o| o.outcome == Outcome::Built);
        let direct_failed = observations.iter().any(|o| o.outcome == Outcome::Failed);
        let dep_failed = observations.iter().any(|o| o.outcome == Outcome::DepFailed);

        // Meta-blocked and not overridden: never attempt (checked before the
        // other knobs, so e.g. `--retry` alone still doesn't build it), and
        // never anything but `Skipped` — the marking masks recorded facts, so a
        // default run behaves identically whatever earlier `--no-skip` runs
        // learned (the report masks the same way; `report::side_state`).
        if skipped && !self.no_skip {
            return Decision::Skipped;
        }
        // A trusted success short-circuits.
        if built {
            return Decision::SkipOk;
        }
        // A known-failing derivation is not worth re-running unless asked. A
        // direct failure is sticky; a dependency block only holds while its
        // culprit is still failing (`!dep_block_stale`).
        if !self.retry {
            if direct_failed {
                return Decision::SkipFail;
            }
            if dep_failed && !dep_block_stale {
                return Decision::SkipFail;
            }
        }
        Decision::Build
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(outcome: Outcome) -> Observation {
        Observation {
            drv_path: "/nix/store/x.drv".into(),
            outcome,
            blocker: Vec::new(),
        }
    }

    #[test]
    fn never_observed_builds() {
        assert_eq!(
            BuildPolicy::default().decide(&[], false, false),
            Decision::Build
        );
    }

    #[test]
    fn recorded_success_skips() {
        // A Built fact — a local build or a recorded cache hit, which the log
        // doesn't distinguish (DESIGN.md §7) — decides SkipOk.
        let o = [obs(Outcome::Built)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false),
            Decision::SkipOk
        );
    }

    #[test]
    fn only_failures_skip_unless_retry() {
        let o = [obs(Outcome::Failed)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false),
            Decision::SkipFail
        );
        let p = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(p.decide(&o, false, false), Decision::Build);
    }

    #[test]
    fn flaky_success_wins() {
        let o = [obs(Outcome::Failed), obs(Outcome::Built)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false),
            Decision::SkipOk
        );
    }

    #[test]
    fn dep_block_holds_until_culprit_heals() {
        // A dependency block (DepFailed, no direct failure) is skipped while its
        // culprit still fails, but re-attempted the moment the block goes stale —
        // no --retry needed. This is the self-healing property (DESIGN.md §5).
        let o = [obs(Outcome::DepFailed)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false),
            Decision::SkipFail
        );
        assert_eq!(
            BuildPolicy::default().decide(&o, false, true),
            Decision::Build
        );
    }

    #[test]
    fn direct_failure_stays_sticky_even_when_a_dep_block_is_stale() {
        // A drv that failed *directly* is sticky regardless of dep staleness: a
        // stale sibling dep-block must not resurrect a real direct failure.
        // --retry is the only escape.
        let o = [obs(Outcome::Failed), obs(Outcome::DepFailed)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, true),
            Decision::SkipFail
        );
        let retry = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(retry.decide(&o, false, true), Decision::Build);
    }

    #[test]
    fn skipped_stays_skipped_unless_no_skip() {
        // Meta-blocked: never attempted by default, not even under --retry.
        let p = BuildPolicy::default();
        assert_eq!(p.decide(&[], true, false), Decision::Skipped);
        let retry = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(
            retry.decide(&[obs(Outcome::Failed)], true, false),
            Decision::Skipped
        );

        // The marking masks recorded facts: even a drv an earlier --no-skip run
        // built (or found in the cache) stays Skipped on a default run, so its
        // behavior doesn't depend on what past runs happened to learn.
        let o = [obs(Outcome::Built)];
        assert_eq!(p.decide(&o, true, false), Decision::Skipped);

        // --no-skip restores the normal policy.
        let ns = BuildPolicy {
            no_skip: true,
            ..Default::default()
        };
        assert_eq!(ns.decide(&[], true, false), Decision::Build);
        assert_eq!(ns.decide(&o, true, false), Decision::SkipOk);
    }
}
