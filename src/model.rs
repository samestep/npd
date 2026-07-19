//! npd core data model.
//!
//! Pure data types the rest of npd is organized around. The guiding decision
//! (see DESIGN.md §2): build facts are keyed on the *derivation path* — the
//! stable identity of a build recipe. It survives failures (unlike an output
//! path) and is shared across commits automatically.
//!
//! Nothing here performs I/O or reads the clock; timestamps are passed in. That
//! keeps the model deterministic and trivially testable and lets the
//! orchestration layer own all the impurity.

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

/// Where a build observation came from. Local builds and substituter presence
/// are both observations in one append-only log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// We ran `nix build` on one of our machines.
    Local,
    /// narinfo presence on a substituter (success only).
    Cache,
}

/// The result of a single build attempt/observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Built,
    /// The derivation itself failed to build.
    Failed,
    /// A (transitive) dependency failed; this drv never ran.
    DepFailed,
}

/// One append-only fact about one derivation, keyed externally by `drv_path`.
///
/// We never overwrite an observation; flakiness is simply multiple observations
/// of the same `drv_path` with differing outcomes. `when` is unix seconds,
/// passed in by the caller (the model never reads the clock).
///
/// `blocker` is populated only for a [`Outcome::DepFailed`]: it is the output
/// paths of the *specific* still-failing dependency that blocked this drv (the
/// "culprit", DESIGN.md §5). It is what makes a dependency-block *self-healing*
/// without re-evaluation: a later run re-checks those paths' store validity
/// offline — no `.drv`, no closure walk — and re-attempts the dependent the
/// moment the culprit has built or been substituted. Empty for every other
/// outcome (and for a `DepFailed` whose culprit wasn't recorded, which is then
/// treated conservatively as still-blocking).
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub drv_path: String,
    pub source: Source,
    pub outcome: Outcome,
    pub when: i64,
    /// Output paths of the culprit dependency for a `DepFailed`; else empty.
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
    /// `substitutable` means a successful output is available from a substituter
    /// (Nix could fetch it without building) — a *success* signal that says
    /// nothing about local reproducibility. `skipped` is the attr's
    /// meta-blocked (broken/unsupported/insecure) bit from the eval.
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
        substitutable: bool,
        skipped: bool,
        dep_block_stale: bool,
    ) -> Decision {
        let local: Vec<&Observation> = observations
            .iter()
            .filter(|o| o.source == Source::Local)
            .collect();
        let local_built = local.iter().any(|o| o.outcome == Outcome::Built);
        let direct_failed = local.iter().any(|o| o.outcome == Outcome::Failed);
        let dep_failed = local.iter().any(|o| o.outcome == Outcome::DepFailed);

        // Meta-blocked and not overridden: never attempt (checked before the
        // other knobs, so e.g. `--retry` alone still doesn't build it). A real
        // fact recorded earlier (a prior `--no-skip` run) still counts.
        if skipped && !self.no_skip {
            return if local_built {
                Decision::SkipOk
            } else {
                Decision::Skipped
            };
        }
        // A trusted success short-circuits.
        if local_built {
            return Decision::SkipOk;
        }
        if substitutable {
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

    fn obs(source: Source, outcome: Outcome) -> Observation {
        Observation {
            drv_path: "/nix/store/x.drv".into(),
            source,
            outcome,
            when: 0,
            blocker: Vec::new(),
        }
    }

    #[test]
    fn never_observed_builds() {
        assert_eq!(
            BuildPolicy::default().decide(&[], false, false, false),
            Decision::Build
        );
    }

    #[test]
    fn substitutable_skips() {
        assert_eq!(
            BuildPolicy::default().decide(&[], true, false, false),
            Decision::SkipOk
        );
    }

    #[test]
    fn local_success_skips() {
        let o = [obs(Source::Local, Outcome::Built)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false, false),
            Decision::SkipOk
        );
    }

    #[test]
    fn only_failures_skip_unless_retry() {
        let o = [obs(Source::Local, Outcome::Failed)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false, false),
            Decision::SkipFail
        );
        let p = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(p.decide(&o, false, false, false), Decision::Build);
    }

    #[test]
    fn flaky_success_wins() {
        let o = [
            obs(Source::Local, Outcome::Failed),
            obs(Source::Local, Outcome::Built),
        ];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false, false),
            Decision::SkipOk
        );
    }

    #[test]
    fn dep_block_holds_until_culprit_heals() {
        // A dependency block (DepFailed, no direct failure) is skipped while its
        // culprit still fails, but re-attempted the moment the block goes stale —
        // no --retry needed. This is the self-healing property (DESIGN.md §5).
        let o = [obs(Source::Local, Outcome::DepFailed)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false, false),
            Decision::SkipFail
        );
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false, true),
            Decision::Build
        );
    }

    #[test]
    fn direct_failure_stays_sticky_even_when_a_dep_block_is_stale() {
        // A drv that failed *directly* is sticky regardless of dep staleness: a
        // stale sibling dep-block must not resurrect a real direct failure.
        // --retry is the only escape.
        let o = [
            obs(Source::Local, Outcome::Failed),
            obs(Source::Local, Outcome::DepFailed),
        ];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false, true),
            Decision::SkipFail
        );
        let retry = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(retry.decide(&o, false, false, true), Decision::Build);
    }

    #[test]
    fn cache_success_does_not_count_as_local() {
        // A recorded Cache success is not a local build; without substitutable we
        // still build (the caller folds a prior Cache-built obs into substitutable).
        let o = [obs(Source::Cache, Outcome::Built)];
        assert_eq!(
            BuildPolicy::default().decide(&o, false, false, false),
            Decision::Build
        );
    }

    #[test]
    fn skipped_stays_skipped_unless_no_skip() {
        // Meta-blocked: never attempted by default — not even when
        // substitutable, and not under --retry alone.
        let p = BuildPolicy::default();
        assert_eq!(p.decide(&[], false, true, false), Decision::Skipped);
        assert_eq!(p.decide(&[], true, true, false), Decision::Skipped);
        let retry = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(
            retry.decide(&[obs(Source::Local, Outcome::Failed)], false, true, false),
            Decision::Skipped
        );

        // A prior forced build's success is still a trusted fact.
        let o = [obs(Source::Local, Outcome::Built)];
        assert_eq!(p.decide(&o, false, true, false), Decision::SkipOk);

        // --no-skip restores the normal policy.
        let ns = BuildPolicy {
            no_skip: true,
            ..Default::default()
        };
        assert_eq!(ns.decide(&[], false, true, false), Decision::Build);
        assert_eq!(ns.decide(&o, false, true, false), Decision::SkipOk);
    }
}
