//! Evaluate a nixpkgs revision into an `attr -> drv` map via `nix-eval-jobs`,
//! cached as one flat file per eval under `evals/` (DESIGN.md §4). This is the
//! first spine primitive (DESIGN.md §6, §9): a pure fact keyed by
//! `(commit, system, profile)`, computed at most once.
//!
//! The revision's source comes from `builtins.fetchGit`, so Nix fetches and
//! caches it in the store — npd manages no worktrees. `nix-eval-jobs` output is
//! parsed by streaming NDJSON straight off the child's stdout (never buffering
//! the whole, meta-heavy output).

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;

use crate::model::{AttrEval, TestJob};

/// Bumped when the eval file format or *how* we invoke `nix-eval-jobs` changes in
/// a way that could alter the stored attr->drv map; cache entries under a
/// different version are ignored (and regenerated), never parsed by newer code —
/// this version tag is the *only* format-change mechanism (no migration code,
/// see CLAUDE.md).
pub const EVAL_VERSION: u32 = 4;

/// The default (and, for now, only) eval profile. npd owns the config so the key
/// stays a short enumerable label rather than arbitrary Nix (DESIGN.md §6). The
/// allow-flags are on so meta-blocked packages still yield a drv + meta rather
/// than throwing — we want their drvpath and the option to build them anyway.
pub const DEFAULT_PROFILE: &str = "default";

fn profile_config(profile: &str) -> Result<&'static str> {
    match profile {
        "default" => Ok("{ allowBroken = true; allowUnfree = true; \
                          allowUnsupportedSystem = true; allowInsecurePredicate = _: true; }"),
        other => bail!("unknown eval profile: {other:?}"),
    }
}

// --- nix-eval-jobs output ---------------------------------------------------

/// The slice of `meta` we consume (from `--meta`): the availability bits
/// nixpkgs' check-meta computes. The profile's allow-flags let these packages
/// evaluate to a drv anyway; the bits say they shouldn't be *built* by default.
#[derive(Deserialize, Default)]
struct RawMeta {
    broken: Option<bool>,
    unsupported: Option<bool>,
    insecure: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawJob {
    attr: String,
    /// The attr path as an array of *unquoted* elements. Preferred over `attr`
    /// (which nix-eval-jobs quotes when an element contains a `.`) when a clean,
    /// dotted label is wanted — see the test eval.
    attr_path: Vec<String>,
    /// `None` when evaluation of the attr errored (the job line carries an
    /// `error` message instead, which we don't keep — `nix log`/re-eval has it).
    drv_path: Option<String>,
    meta: Option<RawMeta>,
}

/// Fold `--meta`'s availability bits into npd's single "meta-blocked" bit: marked
/// broken *or* unsupported-on-this-system *or* insecure. A missing `meta` (an
/// errored attr carries none) reads as not-blocked. Shared by the full-set walk
/// and the targeted test eval so both classify meta the same way.
fn meta_broken(meta: &RawMeta) -> bool {
    meta.broken == Some(true) || meta.unsupported == Some(true) || meta.insecure == Some(true)
}

fn raw_to_attr_eval(raw: RawJob) -> AttrEval {
    AttrEval {
        attr: raw.attr,
        drv_path: raw.drv_path,
        broken: meta_broken(&raw.meta.unwrap_or_default()),
    }
}

// --- running the evaluator --------------------------------------------------

/// Escape a string for embedding inside a Nix `"..."` literal: backslashes,
/// double quotes, and the `${` interpolation opener. (Attr names and store
/// paths virtually never contain these, but the repo path and revision are
/// user input.)
fn nix_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace("${", "\\${")
}

/// Build the whole-package-set Nix expression `nix-eval-jobs` walks. The
/// revision's source is fetched by `builtins.fetchGit`. Interpolants are
/// escaped exactly as in [`build_tests_expr`] — the repo path in particular is
/// user input (`--nixpkgs`).
fn build_expr(repo: &Path, commit: &str, system: &str, profile: &str) -> Result<String> {
    let cfg = profile_config(profile)?;
    Ok(format!(
        "import (builtins.fetchGit {{ url = \"{}\"; rev = \"{}\"; }}) \
         {{ system = \"{}\"; config = {cfg}; }}",
        nix_escape(&repo.display().to_string()),
        nix_escape(commit),
        nix_escape(system),
    ))
}

/// Run one `nix-eval-jobs` invocation with `workers` worker processes, each
/// heap-capped at `per_worker_mb` (nix-eval-jobs restarts a worker that exceeds
/// it, so total memory ≈ `workers * per_worker_mb`). Progress is streamed onto
/// the caller-supplied `pb`, letting several evals share one MultiProgress.
fn run_eval_pb(
    repo: &Path,
    commit: &str,
    system: &str,
    profile: &str,
    workers: usize,
    per_worker_mb: u64,
    pb: &ProgressBar,
) -> Result<Vec<AttrEval>> {
    let expr = build_expr(repo, commit, system, profile)?;
    let short: String = commit.chars().take(12).collect();
    let label = format!("{short} ({system}, {workers}w)");
    stream_jobs(&expr, workers, per_worker_mb, pb, &label, raw_to_attr_eval)
}

/// Run one `nix-eval-jobs --expr <expr>` (with `workers` workers each capped at
/// `per_worker_mb`), streaming its NDJSON stdout through `map_job` into
/// `AttrEval`s and rendering progress onto `pb`. `label` names the run in the
/// progress bar and the integrity-gate error. Shared by the cached full-set eval
/// (`map_job` → [`AttrEval`], keyed on `attr`) and the targeted test eval
/// (`map_job` → [`TestJob`], relabelled from `attrPath`) — both stream the same
/// job shape and want the same truncation gate, so it's generic over the output.
fn stream_jobs<T>(
    expr: &str,
    workers: usize,
    per_worker_mb: u64,
    pb: &ProgressBar,
    label: &str,
    map_job: impl Fn(RawJob) -> T,
) -> Result<Vec<T>> {
    // nix-eval-jobs prints a full Nix traceback per errored attr (megabytes over a
    // whole package set), and the actionable per-attr error is already in the
    // stdout JSON — so we neither inherit its stderr (terminal spam) nor persist
    // it to disk. A thread drains stderr into a bounded ring buffer, keeping only
    // the last few lines for the fatal-error diagnostic below; draining it (vs. an
    // undrained pipe) also can't deadlock while we stream stdout.

    // `--meta` costs ~15% (each package's meta attrset is forced and emitted),
    // but it's what carries `broken`/`unsupported`/`insecure` — the bits the
    // build policy needs to skip meta-blocked packages by default.

    // nix-eval-jobs compares `--max-memory-size` (MiB) against `ru_maxrss`
    // scaled by 1024, which is correct on Linux (KiB) but off by 1024× on
    // macOS, where `ru_maxrss` is in bytes: the effective cap becomes
    // `per_worker_mb` *KiB*, every worker trips it after its first job, and
    // each subsequent job pays a full worker restart + nixpkgs re-import
    // (~100× slower end-to-end). Compensate by passing the cap ×1024 on macOS.
    // Remove once https://github.com/NixOS/nix-eval-jobs/issues (bytes
    // vs KiB in `shouldRestart`, src/worker.cc) is fixed upstream.
    let max_memory_size = if cfg!(target_os = "macos") {
        per_worker_mb * 1024
    } else {
        per_worker_mb
    };
    let mut child = Command::new("nix-eval-jobs")
        .args([
            "--meta",
            "--workers",
            &workers.to_string(),
            "--max-memory-size",
            &max_memory_size.to_string(),
            "--expr",
            expr,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning nix-eval-jobs (on PATH? use the flake dev shell)")?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stderr_tail = thread::spawn(move || {
        const KEEP: usize = 20;
        let mut ring: VecDeque<String> = VecDeque::with_capacity(KEEP + 1);
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if ring.len() == KEEP {
                ring.pop_front();
            }
            ring.push_back(line);
        }
        ring.into_iter().collect::<Vec<_>>().join("\n")
    });

    // A full-set eval takes minutes (and is pathologically slow on macOS — see
    // DESIGN); show a live elapsed timer next to the attr counter (like `nom`'s
    // build timer) so a slow eval reads as "still working", not "hung". The
    // `{elapsed}` field is re-rendered on every steady tick, so it ticks even
    // between attrs; the counter updates as attrs stream in.
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} ⏱ {elapsed} {msg}").expect("valid template"),
    );
    pb.reset_elapsed();
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message(format!("evaluating {label}"));
    let mut attrs = Vec::new();
    for item in serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<RawJob>()
    {
        match item.context("parsing nix-eval-jobs output") {
            Ok(raw) => attrs.push(map_job(raw)),
            Err(e) => {
                // A `Child` is not killed on drop: bail out without reaping and
                // a multi-GB nix-eval-jobs (plus its workers) keeps evaluating
                // into the void. Kill it (which also ends the stderr thread via
                // EOF) before surfacing the parse error.
                let _ = child.kill();
                let _ = child.wait();
                pb.abandon_with_message(format!("eval of {label} failed"));
                return Err(e);
            }
        }
        pb.set_message(format!("evaluating {label} — {} attrs", attrs.len()));
    }

    let status = child.wait().context("waiting for nix-eval-jobs")?;
    let stderr_tail = stderr_tail.join().unwrap_or_default();
    // Integrity gate. Per-attr eval errors are emitted *in band* as JSON
    // (`{"attr":…,"error":…}`) and do NOT affect the exit code — a complete
    // full-set eval exits 0 even with thousands of `throw`n attrs. A non-zero
    // exit means a *fatal* abort: a worker died mid-eval (most often an OOM
    // SIGKILL when the workers' memory caps oversubscribe RAM), in which case
    // the streamed output is silently TRUNCATED — we got some attrs but not
    // all. Caching that would poison every future diff/report with phantom
    // "removed" packages, so we refuse it outright rather than trust a partial.
    if !status.success() {
        pb.abandon_with_message(format!("eval of {label} failed (truncated)"));
        bail!(
            "nix-eval-jobs did not finish evaluating {label}: it exited \
             {status} after streaming {} attr(s), so the result is truncated and \
             will NOT be cached. A worker most likely died — commonly out-of-memory: \
             reduce the worker count or --max-memory-size so their caps fit in RAM. \
             Last stderr:\n{}",
            attrs.len(),
            stderr_tail,
        );
    }
    // Declare success only after the integrity gate: a truncated eval must not
    // flash an "evaluated …" line before the error.
    pb.finish_with_message(format!("evaluated {label} — {} attrs", attrs.len()));
    Ok(attrs)
}

// --- targeted test eval (passthru.tests of the changed set) ------------------
//
// The `--tests` feature (ported from nixpkgs-review#397): for the packages in a
// change's *changed set*, also build their `passthru.tests`. This is a small,
// targeted eval over the (few) changed attrs, distinct from the full-set eval —
// and it *is* cached, per package, in SQLite (see `store::Store` and `main`): a
// test's drv is a pure function of `(commit, system, profile, package-attr)`, so
// `eval_tests` runs only over the packages a run hasn't cached yet (the misses),
// and a fully-cached re-run touches no `nix-eval-jobs` at all. It's a SQLite
// fact, not a flat eval file, because the access pattern is keyed/incremental
// (look up a package, append new ones) rather than the full-set eval's
// bulk/write-once/read-whole-and-diff (DESIGN §4).
//
// The full-set `nix-eval-jobs` walk never reaches these drvs: a package's
// `passthru.tests` is a plain attrset without `recurseForDerivations`, so it's
// not descended into. We surface them with a targeted expression: a job tree
// `<pkg>.tests.<name>` where each package's `.tests` is a *thunk* forced by
// `nix-eval-jobs` in its per-attr worker — so a package that fails to evaluate
// (even an uncatchable parse error `tryEval` can't trap) is isolated to its own
// attr, exactly as in the full-set walk, rather than aborting the whole eval.

/// Nix expression exposing the `passthru.tests` of `attrs` at one revision as a
/// `nix-eval-jobs` job tree. Each requested `<pkg>` becomes a recursable node
/// `{ recurseForDerivations = true; tests = <thunk>; }`; the `tests` thunk (which
/// is what forces the package) is evaluated per-attr in a worker, so a throwing
/// package errors only its own subtree. `tests` resolves to the package's
/// `passthru.tests` — a derivation (emitted as `<pkg>.tests`) or an attrset made
/// recursable (emitted as `<pkg>.tests.<name>`); anything else yields no jobs.
///
/// **Computed meta-blocked bit.** A `passthru.tests` entry is usually a
/// `nixosTest`/`vm-test-run` derivation, which does *not* pass through nixpkgs'
/// `check-meta` `commonMeta`, so — unlike a normal package — its raw `meta`
/// carries no computed `unsupported`/`insecure` field (only whatever the test
/// framework set, e.g. `platforms`). So `--meta` alone can't tell us a test is
/// meta-blocked. `mark` computes it here — platform support via
/// `lib.meta.availableOn`, insecurity via `knownVulnerabilities` — and injects
/// `unsupported`/`insecure` into each test derivation's `meta`, so the same fold
/// the full-set walk uses (`meta_broken`) also classifies tests, matching
/// nixpkgs-review's "marked broken and skipped" (which gets the same answer by
/// `tryEval`-ing the outPath under a strict config). `mark` stops at
/// derivations, so it never forces a derivation's internals, and each recursed
/// leaf is wrapped in `tryEval` so one throwing test errors only itself — the
/// per-leaf isolation nix-eval-jobs would otherwise give the untransformed tree.
fn build_tests_expr(
    repo: &Path,
    commit: &str,
    system: &str,
    profile: &str,
    attrs: &[String],
) -> Result<String> {
    let cfg = profile_config(profile)?;
    let list: String = attrs
        .iter()
        .map(|a| format!("\"{}\" ", nix_escape(a)))
        .collect();
    const TEMPLATE: &str = r#"
let
  pkgs = import (builtins.fetchGit { url = "@REPO@"; rev = "@COMMIT@"; }) { system = "@SYSTEM@"; config = @CFG@; };
  lib = pkgs.lib;
  host = pkgs.stdenv.hostPlatform;
  attrs = [ @ATTRS@];
  # Inject the *computed* meta-blocked bits (see build_tests_expr doc) into every
  # test derivation, recursing through `tests` sub-attrsets. Stops at derivations
  # (never forces their internals); each recursed leaf goes through `tryEval`, so
  # a test that throws when forced is passed through untouched to error on its own.
  mark = t:
    if lib.isDerivation t then
      t // {
        meta = (t.meta or { }) // {
          unsupported = !(lib.meta.availableOn host t);
          insecure = (t.meta.knownVulnerabilities or [ ]) != [ ];
        };
      }
    else if lib.isAttrs t then
      lib.mapAttrs (_: v: let r = builtins.tryEval (mark v); in if r.success then r.value else v) t
      // { recurseForDerivations = true; }
    else t;
  node = name: {
    recurseForDerivations = true;
    # Forced per-attr in a nix-eval-jobs worker: a package that fails to evaluate
    # errors only its own `<pkg>.tests`, never the whole run.
    tests =
      let
        pkg = lib.attrByPath (lib.splitString "." name) null pkgs;
        t = if pkg == null then null else (pkg.tests or null);
      in
        if lib.isDerivation t || lib.isAttrs t then mark t
        else { recurseForDerivations = true; };
  };
in
lib.listToAttrs (map (name: lib.nameValuePair name (node name)) attrs)
// { recurseForDerivations = true; }
"#;
    Ok(TEMPLATE
        .replace("@REPO@", &nix_escape(&repo.display().to_string()))
        .replace("@COMMIT@", &nix_escape(commit))
        .replace("@SYSTEM@", &nix_escape(system))
        .replace("@CFG@", cfg)
        .replace("@ATTRS@", &list))
}

/// Evaluate the `passthru.tests` of `attrs` at `commit`/`system` into [`TestJob`]s
/// (one per resolved `<pkg>.tests.<name>`). This is the *miss* path of the cache:
/// callers pass only the packages not already cached (see `main`). Returns an
/// empty vec for an empty `attrs`.
pub fn eval_tests(
    repo: &Path,
    commit: &str,
    system: &str,
    profile: &str,
    attrs: &[String],
) -> Result<Vec<TestJob>> {
    if attrs.is_empty() {
        return Ok(Vec::new());
    }
    let expr = build_tests_expr(repo, commit, system, profile, attrs)?;
    // A targeted eval over a small changed set: a couple of workers is plenty,
    // and each still re-evaluates the nixpkgs spine, so more would only waste RAM.
    let workers = attrs.len().clamp(1, 4);
    let short: String = commit.chars().take(12).collect();
    let label = format!("tests {short} ({system})");
    let pb = ProgressBar::new_spinner();
    // Label and split from `attrPath` (unquoted elements) rather than `attr`
    // (which nix-eval-jobs quotes for the dotted package component, e.g.
    // `"python3Packages.requests".tests.foo`): element 0 is the package we asked
    // for (the job tree is keyed by it), and the whole path joined is the clean
    // `<pkg>.tests.<name>` label.
    let map = |raw: RawJob| {
        let pkg_attr = raw.attr_path.first().cloned().unwrap_or_default();
        let test_attr = raw.attr_path.join(".");
        let broken = meta_broken(&raw.meta.unwrap_or_default());
        TestJob {
            pkg_attr,
            test_attr,
            drv_path: raw.drv_path,
            broken,
        }
    };
    let r = stream_jobs(&expr, workers, DEFAULT_WORKER_MEM_MB, &pb, &label, map);
    pb.finish_and_clear();
    r
}

// --- concurrency: a memory-slot budget over parallel evals -------------------

/// Default per-worker heap cap (matches nix-eval-jobs' own 4 GiB default).
const DEFAULT_WORKER_MEM_MB: u64 = 4096;
/// A single eval sees diminishing returns past this many workers (each worker
/// redundantly re-evaluates the package-set spine), so cap the auto-derived
/// width here even when the RAM budget could afford more.
const MAX_WORKERS_PER_EVAL: u64 = 8;

/// Optional overrides for the parallel-eval sizing (see [`eval_plan`]). Every
/// field `None` means "auto-size from system RAM"; the CLI surfaces each as a
/// global flag (this struct doubles as the clap group) so the scheme can be
/// tuned point-by-point without env vars.
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct EvalOpts {
    /// RAM budget for parallel evaluation, MiB (default: 80% of *available* RAM).
    #[arg(long)]
    pub mem_budget_mb: Option<u64>,
    /// Per-`nix-eval-jobs`-worker heap cap, MiB (default: 4096).
    #[arg(long)]
    pub worker_mem_mb: Option<u64>,
    /// Number of evaluations to run at once (default: auto from the RAM budget).
    #[arg(long = "eval-concurrency")]
    pub concurrency: Option<u64>,
    /// `nix-eval-jobs` workers per evaluation (default: auto, clamped 1–8).
    #[arg(long = "eval-workers")]
    pub workers: Option<u64>,
}

/// *Currently available* system RAM in MiB (not total), so the auto-sizer never
/// oversubscribes what's actually free — otherwise a machine already using most
/// of its RAM (e.g. a host whose VM holds half of it) would OOM/swap. Falls back
/// to total, then to a conservative 8 GiB, if the available figure isn't
/// obtainable. Linux: `/proc/meminfo`; macOS: `vm_stat` / `sysctl hw.memsize`.
fn available_mem_mb() -> u64 {
    // Linux: MemAvailable (kernel's estimate of allocatable-without-swap); fall
    // back to MemTotal on kernels too old to report it.
    if let Ok(s) = fs::read_to_string("/proc/meminfo") {
        let field = |name: &str| {
            s.lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
        };
        if let Some(kb) = field("MemAvailable:").or_else(|| field("MemTotal:")) {
            return kb / 1024;
        }
    }
    // macOS: reclaimable pages from `vm_stat`, else total from `sysctl`.
    macos_available_mb().or_else(macos_total_mb).unwrap_or(8192)
}

/// macOS available RAM (MiB): free + inactive + speculative + purgeable pages,
/// per `vm_stat`. A heuristic (like Activity Monitor's "available"), but far
/// better than assuming all of `hw.memsize` is free.
fn macos_available_mb() -> Option<u64> {
    let out = Command::new("vm_stat")
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    // Header: "Mach Virtual Memory Statistics: (page size of 16384 bytes)".
    let page = text
        .split("page size of ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(16384);
    let pages = |name: &str| -> u64 {
        text.lines()
            .find(|l| l.trim_start().starts_with(name))
            .and_then(|l| l.rsplit(':').next())
            .and_then(|v| v.trim().trim_end_matches('.').parse::<u64>().ok())
            .unwrap_or(0)
    };
    let avail = pages("Pages free")
        + pages("Pages inactive")
        + pages("Pages speculative")
        + pages("Pages purgeable");
    (avail > 0).then_some(avail * page / 1024 / 1024)
}

/// macOS total physical RAM (MiB) via `sysctl -n hw.memsize` (bytes).
fn macos_total_mb() -> Option<u64> {
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| bytes / 1024 / 1024)
}

/// How to run a batch of `n_jobs` evals: how many at once, how wide each, and
/// the per-worker heap cap. Derived from a RAM budget (default 80% of currently
/// *available* RAM) divided into `per_worker_mb` slots; each knob is overridable
/// via [`EvalOpts`] so the scheme can be benchmarked point-by-point.
struct EvalPlan {
    concurrency: usize,
    workers: usize,
    per_worker_mb: u64,
    budget_mb: u64,
    slots: u64,
}

fn eval_plan(n_jobs: usize, opts: EvalOpts) -> EvalPlan {
    let per_worker_mb = opts.worker_mem_mb.unwrap_or(DEFAULT_WORKER_MEM_MB);
    let budget_mb = opts
        .mem_budget_mb
        .unwrap_or_else(|| available_mem_mb() * 8 / 10);
    let slots = (budget_mb / per_worker_mb).max(1);
    // Run as many evals at once as fit in the budget (but no more than we have),
    // splitting the slots evenly across them; each override wins if set.
    let concurrency = opts
        .concurrency
        .unwrap_or(slots)
        .min(n_jobs.max(1) as u64)
        .max(1);
    let workers = opts
        .workers
        .unwrap_or_else(|| (slots / concurrency).clamp(1, MAX_WORKERS_PER_EVAL))
        .max(1);
    EvalPlan {
        concurrency: concurrency as usize,
        workers: workers as usize,
        per_worker_mb,
        budget_mb,
        slots,
    }
}

/// A counting semaphore (std has none): admits at most `permits` evals at once.
struct Semaphore {
    m: Mutex<usize>,
    cv: Condvar,
}

impl Semaphore {
    fn new(permits: usize) -> Self {
        Semaphore {
            m: Mutex::new(permits),
            cv: Condvar::new(),
        }
    }
    fn acquire(&self) {
        let mut n = self.m.lock().unwrap();
        while *n == 0 {
            n = self.cv.wait(n).unwrap();
        }
        *n -= 1;
    }
    fn release(&self) {
        *self.m.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

// --- cache ------------------------------------------------------------------

pub fn cache_root() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("could not determine cache directory")?
        .join("nix-npd"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(cache_root()?.join("npd.sqlite"))
}

// --- eval files -------------------------------------------------------------
//
// Each eval is a standalone file under `<cache>/evals/`, not SQLite rows. It's a
// bulk, write-once, read-as-a-whole artifact — the only thing we ever do with it
// is diff two of them — so a flat file is both smaller (no per-row / index
// overhead; ~11 MB vs ~22 MB in SQLite) and lets us evict by whole file (drop
// old commits' evals) without vacuuming a monolithic DB. The format is one
// `attr\tdrv` line per attr, sorted by attr (empty drv = no derivation), plus a
// third field `b` on the few rows whose package is marked
// broken/unsupported/insecure, so the diff is a linear two-pointer merge.
//
// The drv column is stored *stripped*: `/nix/store/<h>-<n>.drv` is written as
// just `<h>-<n>` (see `strip_drv`), since that prefix/suffix is constant across
// every line — ~15 B/line, ~15% off the file. Reconstruction (`restore_drv`) is
// one concat per changed row, so it costs nothing on the unchanged majority the
// merge skips. The format is strict — every drv is a `/nix/store` `.drv` or
// absent, matching the rest of npd (e.g. `cache::store_hash`) — with no fallback
// for other shapes: changing it is an EVAL_VERSION bump, so old files are
// ignored and regenerated, never mis-parsed as if they were stripped.
//
// The whole (stripped) TSV is then zstd-compressed on disk (~3x smaller; a full
// study weighed the level and alternatives — a two-file split, higher levels —
// and landed on the default). We diff by reading a file whole and decompressing
// it, so a single stream is the right shape; the merge is unchanged.

fn eval_path(commit: &str, system: &str, profile: &str) -> Result<PathBuf> {
    Ok(cache_root()?.join("evals").join(format!(
        "{commit}-{system}-{profile}-v{EVAL_VERSION}.tsv.zst"
    )))
}

/// Write an eval to its file, sorted by attr, zstd-compressed, atomically: a
/// uniquely-named temp file in the *same directory* (rename is only atomic
/// within one filesystem, so the system temp dir won't do), then rename into
/// place. A crash can never leave a truncated file that would poison the cache,
/// and concurrent writers of the same key can't tread on each other's temp.
fn write_eval(path: &Path, attrs: &[AttrEval]) -> Result<()> {
    let mut rows: Vec<(&str, &str, bool)> = attrs
        .iter()
        .map(|a| {
            (
                a.attr.as_str(),
                a.drv_path.as_deref().map(strip_drv).unwrap_or(""),
                a.broken,
            )
        })
        .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut buf = String::with_capacity(rows.len() * 96);
    for (attr, drv, broken) in rows {
        buf.push_str(attr);
        buf.push('\t');
        buf.push_str(drv);
        // A third field only on the (few) meta-blocked rows: `b`.
        if broken {
            buf.push_str("\tb");
        }
        buf.push('\n');
    }
    // Level 0 = zstd's default level (currently 3); pass the sentinel rather than
    // a number so we track the library's default rather than pinning it.
    let compressed = zstd::encode_all(buf.as_bytes(), 0).context("compressing eval")?;
    let dir = path.parent().expect("eval path has a parent");
    fs::create_dir_all(dir).context("creating evals dir")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).context("creating temp eval file")?;
    tmp.write_all(&compressed)
        .context("writing temp eval file")?;
    tmp.persist(path).context("renaming eval into place")?;
    Ok(())
}

/// Read and decompress an eval file into its TSV text.
fn read_eval(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let tsv = zstd::decode_all(&bytes[..])
        .with_context(|| format!("decompressing {}", path.display()))?;
    String::from_utf8(tsv).with_context(|| format!("{} is not valid UTF-8", path.display()))
}

/// The on-disk form of a drv path: strip the constant `/nix/store/` prefix and
/// `.drv` suffix; [`restore_drv`] re-adds them. Every drv `nix-eval-jobs` emits
/// has this exact shape (an errored attr carries no drv and is stored as an empty
/// field, so this is only ever called on a real path).
fn strip_drv(drv: &str) -> &str {
    let stripped = drv
        .strip_prefix("/nix/store/")
        .and_then(|s| s.strip_suffix(".drv"));
    debug_assert!(
        stripped.is_some(),
        "drv not /nix/store/<hash>-<name>.drv: {drv}"
    );
    stripped.unwrap_or(drv)
}

/// Reconstruct a full drv path from its stored (stripped) form — see [`strip_drv`].
fn restore_drv(field: Option<&str>) -> Option<String> {
    field.map(|s| format!("/nix/store/{s}.drv"))
}

/// One parsed eval row, borrowing from the file buffer: attr, stored-form drv,
/// and the meta-blocked bit.
type EvalRow<'a> = (&'a str, Option<&'a str>, bool);

/// Parse an eval file's bytes into [`EvalRow`]s, borrowing from `buf` (no
/// per-attr allocation). The drv is left in its stored form (see [`strip_drv`]);
/// since that encoding is injective, the merge can compare stored fields
/// directly and only [`restore_drv`] the few rows it emits. Assumes the file is
/// already sorted by attr.
fn parse_eval(buf: &str) -> Vec<EvalRow<'_>> {
    buf.lines()
        .map(|l| {
            let mut fields = l.splitn(3, '\t');
            let attr = fields.next().unwrap_or(l);
            let drv = fields.next().unwrap_or("");
            let broken = fields.next() == Some("b");
            (attr, if drv.is_empty() { None } else { Some(drv) }, broken)
        })
        .collect()
}

/// One changed attr between two evals: its drv and meta-blocked bit on each side
/// (`None` = absent/no derivation there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedAttr {
    pub attr: String,
    pub base_drv: Option<String>,
    pub head_drv: Option<String>,
    pub base_broken: bool,
    pub head_broken: bool,
}

/// The changed set between two cached evals — one [`ChangedAttr`] for each attr
/// whose drv *or* meta-blocked bit differs (meta isn't part of the drv hash, so
/// (un)marking a package broken can change nothing but the bit — still a review
/// event worth a row) — via a linear two-pointer merge over the two sorted
/// files. Only the (few) changed rows are allocated.
pub fn changed_set(
    base: &str,
    head: &str,
    system: &str,
    profile: &str,
) -> Result<Vec<ChangedAttr>> {
    let bp = eval_path(base, system, profile)?;
    let hp = eval_path(head, system, profile)?;
    let bbuf = read_eval(&bp)?;
    let hbuf = read_eval(&hp)?;
    let b = parse_eval(&bbuf);
    let h = parse_eval(&hbuf);

    // One side only: absent on the other (an attr with no drv — an eval error —
    // is treated as absent, exactly as before).
    let base_only = |r: &EvalRow| ChangedAttr {
        attr: r.0.to_string(),
        base_drv: restore_drv(r.1),
        head_drv: None,
        base_broken: r.2,
        head_broken: false,
    };
    let head_only = |r: &EvalRow| ChangedAttr {
        attr: r.0.to_string(),
        base_drv: None,
        head_drv: restore_drv(r.1),
        base_broken: false,
        head_broken: r.2,
    };

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < b.len() && j < h.len() {
        match b[i].0.cmp(h[j].0) {
            std::cmp::Ordering::Less => {
                if b[i].1.is_some() {
                    out.push(base_only(&b[i]));
                }
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                if h[j].1.is_some() {
                    out.push(head_only(&h[j]));
                }
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if b[i].1 != h[j].1 || b[i].2 != h[j].2 {
                    out.push(ChangedAttr {
                        attr: b[i].0.to_string(),
                        base_drv: restore_drv(b[i].1),
                        head_drv: restore_drv(h[j].1),
                        base_broken: b[i].2,
                        head_broken: h[j].2,
                    });
                }
                i += 1;
                j += 1;
            }
        }
    }
    for k in &b[i..] {
        if k.1.is_some() {
            out.push(base_only(k));
        }
    }
    for k in &h[j..] {
        if k.1.is_some() {
            out.push(head_only(k));
        }
    }
    Ok(out)
}

/// Ensure every `(commit, system)` pair has a cached eval file, computing the
/// misses concurrently under a RAM-slot budget (see [`eval_plan`]) — no
/// oversubscription, no killing in-flight work.
pub fn eval_pairs(
    repo: &Path,
    pairs: &[(String, String)],
    profile: &str,
    opts: EvalOpts,
) -> Result<()> {
    let mut todo: Vec<usize> = Vec::new();
    for (i, (commit, system)) in pairs.iter().enumerate() {
        if !eval_path(commit, system, profile)?.exists() {
            todo.push(i);
        }
    }
    if todo.is_empty() {
        return Ok(());
    }

    let plan = eval_plan(todo.len(), opts);
    let sem = Semaphore::new(plan.concurrency);
    let mp = MultiProgress::new();
    // Print through the MultiProgress so this shares indicatif's stderr surface
    // and terminal handling (drawn above the bars on a TTY, hidden otherwise)
    // rather than a bare eprintln! that would bypass and tear the progress bars.
    let _ = mp.println(format!(
        "  eval plan: {} job(s), budget {}MB / {}MB per worker = {} slot(s) \
         -> {} concurrent x {} worker(s)",
        todo.len(),
        plan.budget_mb,
        plan.per_worker_mb,
        plan.slots,
        plan.concurrency,
        plan.workers,
    ));
    thread::scope(|s| -> Result<()> {
        let mut handles = Vec::new();
        for &i in &todo {
            let (commit, system) = (&pairs[i].0, &pairs[i].1);
            let pb = mp.add(ProgressBar::new_spinner());
            let sem = &sem;
            handles.push(s.spawn(move || -> Result<()> {
                sem.acquire();
                let r = run_eval_pb(
                    repo,
                    commit,
                    system,
                    profile,
                    plan.workers,
                    plan.per_worker_mb,
                    &pb,
                );
                sem.release();
                // Persist as soon as this eval completes (the write is atomic):
                // a full-set eval costs minutes, and a *sibling* eval failing —
                // e.g. one OOM among four — must not discard finished work.
                write_eval(&eval_path(commit, system, profile)?, &r?)
            }));
        }
        // Join everything before propagating the first error, so no result is
        // dropped mid-write and every progress bar reaches a final state.
        let mut result = Ok(());
        for h in handles {
            let r = h.join().expect("eval thread panicked");
            if result.is_ok() {
                result = r;
            }
        }
        result
    })
}

/// Ensure both commits are evaluated across all systems (they run concurrently).
pub fn eval_two(
    repo: &Path,
    base: &str,
    head: &str,
    systems: &[String],
    profile: &str,
    opts: EvalOpts,
) -> Result<()> {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(systems.len() * 2);
    for s in systems {
        pairs.push((base.to_string(), s.clone()));
    }
    for s in systems {
        pairs.push((head.to_string(), s.clone()));
    }
    eval_pairs(repo, &pairs, profile, opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stream NDJSON values off `reader`, mapping each to an `AttrEval` — the
    /// same job-parse the production streamer does inline, exercised here over a
    /// fixed buffer rather than a live `nix-eval-jobs` child.
    fn parse_jobs<R: std::io::Read>(reader: R) -> Result<Vec<AttrEval>> {
        let mut out = Vec::new();
        for item in serde_json::Deserializer::from_reader(reader).into_iter::<RawJob>() {
            let raw = item.context("parsing nix-eval-jobs output")?;
            out.push(raw_to_attr_eval(raw));
        }
        Ok(out)
    }

    #[test]
    fn parses_success_broken_and_error_lines() {
        // Any of meta.broken/unsupported/insecure folds into the one `broken`
        // bit; an errored attr has no drvPath (and no meta). Unknown fields
        // (system, fatal, …) are simply ignored.
        let stdout = concat!(
            r#"{"attr":"hello","attrPath":["hello"],"drvPath":"/nix/store/a-hello.drv","meta":{"broken":false,"unsupported":false},"system":"aarch64-linux"}"#,
            "\n",
            r#"{"attr":"br","attrPath":["br"],"drvPath":"/nix/store/b-br.drv","meta":{"broken":true}}"#,
            "\n",
            r#"{"attr":"unsup","attrPath":["unsup"],"drvPath":"/nix/store/c-unsup.drv","meta":{"unsupported":true}}"#,
            "\n",
            r#"{"attr":"bad","attrPath":["bad"],"error":"boom","fatal":false}"#,
            "\n",
        );
        let attrs = parse_jobs(stdout.as_bytes()).unwrap();
        assert_eq!(attrs.len(), 4);

        assert_eq!(attrs[0].attr, "hello");
        assert_eq!(attrs[0].drv_path.as_deref(), Some("/nix/store/a-hello.drv"));
        assert!(!attrs[0].broken);

        assert!(attrs[1].broken);
        assert!(attrs[1].drv_path.is_some());
        assert!(attrs[2].broken);

        assert_eq!(attrs[3].attr, "bad");
        assert_eq!(attrs[3].drv_path, None);
        assert!(!attrs[3].broken);
    }

    #[test]
    fn drv_paths_round_trip_stripped() {
        // A drv loses its prefix/suffix on disk...
        assert_eq!(strip_drv("/nix/store/abc-hello.drv"), "abc-hello");
        assert_eq!(
            restore_drv(Some("abc-hello")).as_deref(),
            Some("/nix/store/abc-hello.drv")
        );
        // ...and strip -> restore is the identity for any /nix/store drv.
        for drv in ["/nix/store/abc-hello.drv", "/nix/store/d.drv"] {
            assert_eq!(restore_drv(Some(strip_drv(drv))).as_deref(), Some(drv));
        }
        // No drv (errored attr) is None on both sides.
        assert_eq!(restore_drv(None), None);
    }

    #[test]
    fn write_eval_strips_and_parse_restores() {
        let ae = |attr: &str, drv: Option<&str>, broken: bool| AttrEval {
            attr: attr.into(),
            drv_path: drv.map(str::to_string),
            broken,
        };
        let attrs = [
            ae("hello", Some("/nix/store/a-hello.drv"), false),
            ae("br", Some("/nix/store/b-br.drv"), true),
            ae("bad", None, false),
        ];
        let dir = std::env::temp_dir().join(format!("npd-eval-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("e.tsv");
        write_eval(&path, &attrs).unwrap();

        // On disk the drv is stripped; a no-derivation attr is an empty field;
        // only the meta-blocked row carries the third `b` field (sorted by attr:
        // bad, br, hello). The file is zstd-compressed, so read it back through
        // the same helper the diff uses.
        let raw = read_eval(&path).unwrap();
        assert_eq!(raw, "bad\t\nbr\tb-br\tb\nhello\ta-hello\n");

        // Parsing + restoring recovers the original rows exactly.
        let parsed = parse_eval(&raw);
        let restored: Vec<_> = parsed
            .iter()
            .map(|(a, d, br)| (*a, restore_drv(*d), *br))
            .collect();
        assert_eq!(restored[0], ("bad", None, false));
        assert_eq!(
            restored[1],
            ("br", Some("/nix/store/b-br.drv".into()), true)
        );
        assert_eq!(
            restored[2],
            ("hello", Some("/nix/store/a-hello.drv".into()), false)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_expr_fetches_and_imports() {
        let repo = Path::new("/repo");
        let full = build_expr(repo, "abc123", "aarch64-linux", "default").unwrap();
        assert!(full.contains(r#"builtins.fetchGit { url = "/repo"; rev = "abc123"; }"#));
        assert!(full.contains("allowBroken = true"));
    }

    #[test]
    fn available_mem_is_sane() {
        // Whatever the platform, we must get a positive figure (never the 8 GiB
        // fallback silently masking a probe bug on the CI host), and on Linux it
        // can't exceed total RAM.
        let avail = available_mem_mb();
        assert!(avail > 0);
        if let Ok(s) = fs::read_to_string("/proc/meminfo")
            && let Some(total) = s
                .lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|n| n.parse::<u64>().ok())
        {
            assert!(avail <= total / 1024, "available {avail} MiB > total");
        }
    }
}
