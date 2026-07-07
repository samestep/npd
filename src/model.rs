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

use serde::{Deserialize, Serialize};

/// Whether an attribute evaluates to something buildable on a platform.
/// Determined purely by evaluation, before any build is attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Existence {
    /// Attr path not present in this revision (determined at diff time by set
    /// membership — `nix-eval-jobs` simply doesn't emit a line for it).
    Absent,
    /// Present but meta.broken / badPlatforms / unsupported / insecure. We still
    /// get a `drv_path` (npd evaluates with the allow-flags on), so it can be
    /// built via a bypass; it's just marked not-to-build.
    Blocked,
    /// Present and eligible to build on this platform.
    Buildable,
    /// Present but evaluation itself errored (no drv) — e.g. an assertion or IFD
    /// failure that survives the allow-flags. Distinct from a *build* failure.
    Error,
}

/// Result of evaluating one attribute on one platform at one commit.
///
/// Pure fact: fully determined by (commit, system, config). `drv_path` is set
/// iff `existence` is `Buildable`. Meta flags are cached for later
/// classification; `None` means we did not (or could not) determine them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttrEval {
    pub attr: String,
    pub existence: Existence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drv_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broken: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub insecure: Option<bool>,
    /// Per meta.hydraPlatforms for this system — whether Hydra is *expected* to build it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hydra_platforms_ok: Option<bool>,
    /// The evaluation error message, when `existence` is `Error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Where a build observation came from. Local builds, Hydra job records, and
/// substituter presence are *all* observations in one append-only log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// We ran `nix build` on one of our machines.
    Local,
    /// Hydra's build record for a named job (forward lookup).
    HydraJob,
    /// narinfo presence on a substituter (success only).
    Cache,
}

/// The result of a single build attempt/observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Built,
    /// The derivation itself failed to build.
    Failed,
    /// A (transitive) dependency failed; this drv never ran.
    DepFailed,
    /// The source has no record (narinfo 404, queued job, ...).
    NotAttempted,
}

/// One append-only fact about one derivation, keyed externally by `drv_path`.
///
/// We never overwrite an observation; flakiness is simply multiple observations
/// of the same `drv_path` with differing outcomes. `when` is unix seconds,
/// passed in by the caller (the model never reads the clock).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub drv_path: String,
    pub source: Source,
    pub outcome: Outcome,
    pub when: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// ~0 for a substituted/cached result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
    /// Hydra `isCachedBuild` / substituted rather than genuinely run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Path under `$NPD_STATE/logs`, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_ref: Option<String>,
    /// Hydra build id, when `source` is `HydraJob`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<u64>,
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
}

/// Turns a derivation's observation history into an action.
///
/// The ergonomic core (DESIGN.md §5): the cache-bypass knobs are just fields,
/// and [`BuildPolicy::decide`] is a pure predicate over the append-only log
/// plus whether the output is substitutable.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildPolicy {
    /// Rebuild even a previously-succeeded drv (suspect a flaky success).
    pub recheck: bool,
    /// Re-attempt a previously-failed drv (expect it might pass now).
    pub retry: bool,
    /// Ignore Cache/Hydra success; require a genuine local build.
    pub prefer_local: bool,
}

impl BuildPolicy {
    /// Decide whether to build `drv_path` given its observations.
    ///
    /// `substitutable` means a successful output is available from a substituter
    /// (Nix could fetch it without building) — a *success* signal that says
    /// nothing about local reproducibility.
    pub fn decide(&self, observations: &[Observation], substitutable: bool) -> Decision {
        let local: Vec<&Observation> = observations
            .iter()
            .filter(|o| o.source == Source::Local)
            .collect();
        let local_built = local.iter().any(|o| o.outcome == Outcome::Built);
        let local_failed_only = !local.is_empty()
            && local
                .iter()
                .all(|o| matches!(o.outcome, Outcome::Failed | Outcome::DepFailed));

        // A trusted success short-circuits unless we're deliberately re-checking.
        if local_built && !self.recheck {
            return Decision::SkipOk;
        }
        if substitutable && !self.prefer_local && !self.recheck {
            return Decision::SkipOk;
        }
        // A known-failing derivation is not worth re-running unless asked.
        if local_failed_only && !self.retry {
            return Decision::SkipFail;
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
            system: None,
            duration_s: None,
            cached: None,
            machine: None,
            log_ref: None,
            build_id: None,
        }
    }

    #[test]
    fn never_observed_builds() {
        assert_eq!(BuildPolicy::default().decide(&[], false), Decision::Build);
    }

    #[test]
    fn substitutable_skips_unless_prefer_local() {
        assert_eq!(BuildPolicy::default().decide(&[], true), Decision::SkipOk);
        let p = BuildPolicy {
            prefer_local: true,
            ..Default::default()
        };
        assert_eq!(p.decide(&[], true), Decision::Build);
    }

    #[test]
    fn local_success_skips_unless_recheck() {
        let o = [obs(Source::Local, Outcome::Built)];
        assert_eq!(BuildPolicy::default().decide(&o, false), Decision::SkipOk);
        let p = BuildPolicy {
            recheck: true,
            ..Default::default()
        };
        assert_eq!(p.decide(&o, false), Decision::Build);
    }

    #[test]
    fn only_failures_skip_unless_retry() {
        let o = [obs(Source::Local, Outcome::Failed)];
        assert_eq!(BuildPolicy::default().decide(&o, false), Decision::SkipFail);
        let p = BuildPolicy {
            retry: true,
            ..Default::default()
        };
        assert_eq!(p.decide(&o, false), Decision::Build);
    }

    #[test]
    fn flaky_success_wins() {
        let o = [
            obs(Source::Local, Outcome::Failed),
            obs(Source::Local, Outcome::Built),
        ];
        assert_eq!(BuildPolicy::default().decide(&o, false), Decision::SkipOk);
    }

    #[test]
    fn hydra_success_does_not_count_as_local() {
        // A Hydra/Cache success is not a local build; without substitutable we still build.
        let o = [obs(Source::HydraJob, Outcome::Built)];
        assert_eq!(BuildPolicy::default().decide(&o, false), Decision::Build);
    }
}
