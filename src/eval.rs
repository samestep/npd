//! Run `nix-eval-jobs` and schedule the runs: evaluate a nixpkgs revision into
//! an `attr -> drv` map — the first spine primitive (DESIGN.md §6, §9), a pure
//! fact keyed by `(commit, system)`, computed at most once and cached as one
//! flat file per eval (the file format and its diff live in [`crate::evalfile`]).
//!
//! The revision's source comes from `builtins.fetchGit`, so Nix fetches and
//! caches it in the store — npd manages no worktrees. `nix-eval-jobs` output is
//! parsed by streaming NDJSON straight off the child's stdout (never buffering
//! the whole, meta-heavy output).

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;

use crate::evalfile::{eval_path, write_eval};
use crate::model::{AttrEval, TestJob};

/// The one nixpkgs config every eval runs under. npd owns the config
/// (DESIGN.md §6), which is what makes the eval cache key just
/// `(commit, system)` — changing this line changes the attr→drv map, so it is
/// by definition an [`crate::evalfile::EVAL_VERSION`] bump. The allow-flags are
/// on so meta-blocked packages still yield a drv + meta rather than throwing —
/// we want their drvpath and the option to build them anyway.
const EVAL_CONFIG: &str = "{ allowBroken = true; allowUnfree = true; \
                             allowUnsupportedSystem = true; allowInsecurePredicate = _: true; }";

// --- nix-eval-jobs output ---------------------------------------------------

/// The slice of `meta` we consume (from `--meta`): the availability bits
/// nixpkgs' check-meta computes. [`EVAL_CONFIG`]'s allow-flags let these packages
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
    /// `error` message instead, which we don't keep — re-evaluating reproduces it).
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
fn build_expr(repo: &Path, commit: &str, system: &str) -> String {
    format!(
        "import (builtins.fetchGit {{ url = \"{}\"; rev = \"{}\"; }}) \
         {{ system = \"{}\"; config = {EVAL_CONFIG}; }}",
        nix_escape(&repo.display().to_string()),
        nix_escape(commit),
        nix_escape(system),
    )
}

/// Scrub the evaluator's environment of the variables nixpkgs is known to
/// leak into derivations via `builtins.getEnv` (drbd bakes `$SHELL` into its
/// Makefile patch), so cached evals don't depend on the shell npd was launched
/// from — `getEnv` then returns `""`, matching a hermetic evaluation.
fn scrub_env(cmd: &mut Command) -> &mut Command {
    cmd.env_remove("SHELL");
    cmd
}

/// Run one `nix-eval-jobs --expr <expr>` (with `workers` workers each capped at
/// `per_worker_mb`), streaming its NDJSON stdout through `map_job` into a vec.
/// `on_item` fires per streamed job so callers can render progress however they
/// like; `label` names the run in the integrity-gate error. Shared by the
/// sharded full-set eval (`map_job` → [`AttrEval`]) and the targeted test eval
/// (`map_job` → [`TestJob`], relabelled from `attrPath`) — both stream the same
/// job shape and want the same truncation gate, so it's generic over the output.
fn stream_jobs<T>(
    expr: &str,
    workers: usize,
    per_worker_mb: u64,
    label: &str,
    map_job: impl Fn(RawJob) -> T,
    mut on_item: impl FnMut(),
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
    // Fixed upstream (https://github.com/NixOS/nix-eval-jobs/issues/425, via
    // https://github.com/NixOS/nix-eval-jobs/pull/426); remove once the
    // nix-eval-jobs on PATH carries that fix.
    let max_memory_size = if cfg!(target_os = "macos") {
        per_worker_mb * 1024
    } else {
        per_worker_mb
    };
    let mut child = scrub_env(Command::new("nix-eval-jobs").args([
        "--meta",
        "--workers",
        &workers.to_string(),
        "--max-memory-size",
        &max_memory_size.to_string(),
        "--expr",
        expr,
    ]))
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
                return Err(e);
            }
        }
        on_item();
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
    // The [`EvalAborted`] marker is what lets the scheduler requeue the shard.
    if !status.success() {
        return Err(anyhow::Error::new(EvalAborted).context(format!(
            "nix-eval-jobs did not finish evaluating {label}: it exited \
             {status} after streaming {} attr(s), so the result is truncated and \
             will NOT be cached. A worker most likely died — commonly out-of-memory. \
             Last stderr:\n{}",
            attrs.len(),
            stderr_tail,
        )));
    }
    Ok(attrs)
}

// --- targeted test eval (passthru.tests of the changed set) ------------------
//
// The `--tests` feature (ported from nixpkgs-review#397): for the packages in a
// change's *changed set*, also build their `passthru.tests`. This is a small,
// targeted eval over the (few) changed attrs, distinct from the full-set eval —
// and it *is* cached, per package, in SQLite (see `store::Store` and `main`): a
// test's drv is a pure function of `(commit, system, package-attr)`, so
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
fn build_tests_expr(repo: &Path, commit: &str, system: &str, attrs: &[String]) -> String {
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
    TEMPLATE
        .replace("@REPO@", &nix_escape(&repo.display().to_string()))
        .replace("@COMMIT@", &nix_escape(commit))
        .replace("@SYSTEM@", &nix_escape(system))
        .replace("@CFG@", EVAL_CONFIG)
        .replace("@ATTRS@", &list)
}

/// Evaluate the `passthru.tests` of `attrs` at `commit`/`system` into [`TestJob`]s
/// (one per resolved `<pkg>.tests.<name>`). This is the *miss* path of the cache:
/// callers pass only the packages not already cached (see `main`). Returns an
/// empty vec for an empty `attrs`.
pub fn eval_tests(
    repo: &Path,
    commit: &str,
    system: &str,
    attrs: &[String],
) -> Result<Vec<TestJob>> {
    if attrs.is_empty() {
        return Ok(Vec::new());
    }
    let expr = build_tests_expr(repo, commit, system, attrs);
    // A targeted eval over a small changed set: a couple of workers is plenty,
    // and each still re-evaluates the nixpkgs spine, so more would only waste RAM.
    let workers = attrs.len().clamp(1, 4);
    let short: String = commit.chars().take(12).collect();
    let label = format!("tests {short} ({system})");
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} ⏱ {elapsed} {msg}").expect("valid template"),
    );
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message(format!("evaluating {label}"));
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
    let mut n = 0usize;
    let r = stream_jobs(&expr, workers, DEFAULT_WORKER_MEM_MB, &label, map, || {
        n += 1;
        pb.set_message(format!("evaluating {label} — {n} tests"));
    });
    pb.finish_and_clear();
    r
}

// --- scheduling: one queue of shards (DESIGN §6) ------------------------------

/// Default per-worker heap cap (matches nix-eval-jobs' own 4 GiB default).
const DEFAULT_WORKER_MEM_MB: u64 = 4096;

/// Top-level attr names per shard. Larger shards amortize the per-job nixpkgs
/// import (a few seconds each); smaller ones requeue more cheaply and balance
/// better. At ~400 names a typical shard runs tens of seconds at one worker,
/// putting the import tax in the single-digit percent range (measured).
const NAMES_PER_SHARD: usize = 400;

/// Optional overrides for the eval scheduler; `None` = auto from the machine's
/// invariants (see [`eval_slots`]).
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct EvalOpts {
    /// Concurrent shard evaluations (default: min(cores, total RAM / worker cap)).
    #[arg(long = "eval-slots")]
    pub slots: Option<u64>,
    /// Per-`nix-eval-jobs`-worker heap cap, MiB (default: 4096).
    #[arg(long)]
    pub worker_mem_mb: Option<u64>,
}

/// A fatal `nix-eval-jobs` abort (non-zero exit): the streamed output was
/// truncated and discarded. A marker type so the scheduler can recognize it
/// through the anyhow chain and requeue the shard at reduced concurrency.
#[derive(Debug)]
struct EvalAborted;

impl std::fmt::Display for EvalAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "nix-eval-jobs aborted before finishing (output truncated)"
        )
    }
}

impl std::error::Error for EvalAborted {}

/// Total physical RAM in MiB. Unlike *available* RAM (which the old planner
/// used, and which lies — it moves while a minutes-long eval runs), this is an
/// invariant of the machine, so the width heuristic may plan from it. Linux:
/// `/proc/meminfo MemTotal`; macOS: `sysctl -n hw.memsize`; else 8 GiB.
fn total_mem_mb() -> u64 {
    if let Ok(s) = fs::read_to_string("/proc/meminfo")
        && let Some(kb) = s
            .lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|n| n.parse::<u64>().ok())
    {
        return kb / 1024;
    }
    Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| bytes / 1024 / 1024)
        .unwrap_or(8192)
}

/// The number of concurrent shard jobs to start with: the user's
/// `--eval-slots` if given, else bounded by the machine's *invariants* — one
/// worker per slot, so cores, and total RAM divided by the per-worker heap
/// cap. The dynamic part of RAM is handled by feedback, not planning: the
/// queue sheds slots when a shard is OOM-killed ([`eval_pairs`]).
fn eval_slots(cores: usize, mem_mb: u64, per_worker_mb: u64, user: Option<u64>) -> usize {
    match user {
        Some(s) => (s as usize).max(1),
        None => cores.min((mem_mb / per_worker_mb.max(1)).max(1) as usize),
    }
}

/// The top-level attr names of the package set at `(commit, system)` — the
/// space the shards partition. Cheap (well under a second warm): forcing
/// `attrNames` touches no derivations. The literal `recurseForDerivations`
/// key is dropped to mirror how `nix-eval-jobs` skips it when walking a set.
fn enumerate_names(repo: &Path, commit: &str, system: &str) -> Result<Vec<String>> {
    let expr = format!("builtins.attrNames ({})", build_expr(repo, commit, system));
    let out = scrub_env(
        Command::new("nix-instantiate").args(["--eval", "--strict", "--json", "-E", &expr]),
    )
    .output()
    .context("running nix-instantiate (attr names)")?;
    if !out.status.success() {
        bail!(
            "enumerating top-level attrs of {commit} ({system}) failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let mut names: Vec<String> =
        serde_json::from_slice(&out.stdout).context("parsing attr names")?;
    names.retain(|n| n != "recurseForDerivations");
    Ok(names)
}

/// The eval expression for one shard: the same import as the whole-set walk,
/// narrowed to `names` via `listToAttrs`. Each value stays a thunk forced
/// per-attr in the worker, so walk semantics and error isolation match the
/// monolithic root exactly — validated byte-for-byte against a whole-set eval
/// (DESIGN §6).
fn shard_expr(repo: &Path, commit: &str, system: &str, names: &[String]) -> String {
    let list: String = names
        .iter()
        .map(|n| format!("\"{}\" ", nix_escape(n)))
        .collect();
    format!(
        "let pkgs = {}; in builtins.listToAttrs \
         (map (n: {{ name = n; value = pkgs.${{n}}; }}) [ {list}])",
        build_expr(repo, commit, system)
    )
}

/// Where one shard's completed rows persist until its whole eval is assembled
/// (then the eval's `partial/` dir is deleted). Content-addressed by the
/// shard's names, so a change in sharding layout simply misses rather than
/// resuming from the wrong rows. This is what makes an interrupted (^C, OOM,
/// crash) eval resumable: only unfinished shards re-run.
fn partial_path(commit: &str, system: &str, names: &[String]) -> Result<PathBuf> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    names.hash(&mut h);
    Ok(crate::paths::cache_root()?
        .join("evals")
        .join("partial")
        .join(format!(
            "{commit}-{system}-v{}",
            crate::evalfile::EVAL_VERSION
        ))
        .join(format!("{:016x}.tsv", h.finish())))
}

/// One unit of work: a slice of one eval's top-level names.
struct Shard {
    /// Index into the per-eval state vec.
    eval: usize,
    /// Range within that eval's name list.
    names: std::ops::Range<usize>,
}

/// Ensure every `(commit, system)` pair has a cached eval file, via **one
/// global queue of shards** (DESIGN §6): each eval's top-level names are
/// split into [`NAMES_PER_SHARD`] slices, and every shard is an independent
/// one-worker `nix-eval-jobs` job. [`eval_slots`] jobs run at once; a shard
/// that aborts (in practice a worker OOM-kill) is simply requeued while the
/// slot count backs off multiplicatively — and creeps back up on sustained
/// success (AIMD). Completed shards persist immediately ([`partial_path`]),
/// so any interruption resumes at shard granularity; when an eval's last
/// shard lands, its rows are assembled and written as the one cached file.
pub fn eval_pairs(repo: &Path, pairs: &[(String, String)], opts: EvalOpts) -> Result<()> {
    let mut todo: Vec<usize> = Vec::new();
    // Dedupe: `npd X X` (or repeated --system) would otherwise run the same
    // eval twice concurrently — harmless (the write is atomic) but 2× the work.
    let mut seen = std::collections::HashSet::new();
    for (i, (commit, system)) in pairs.iter().enumerate() {
        if !eval_path(commit, system)?.exists() && seen.insert((commit, system)) {
            todo.push(i);
        }
    }
    if todo.is_empty() {
        return Ok(());
    }

    let per_worker_mb = opts.worker_mem_mb.unwrap_or(DEFAULT_WORKER_MEM_MB);
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let slots = eval_slots(cores, total_mem_mb(), per_worker_mb, opts.slots);
    let mp = MultiProgress::new();

    /// Per-eval bookkeeping shared by the queue workers.
    struct Eval<'a> {
        commit: &'a str,
        system: &'a str,
        label: String,
        names: Vec<String>,
        shards_total: usize,
        shards_done: AtomicUsize,
        attrs_done: AtomicUsize,
        rows: Mutex<Vec<AttrEval>>,
        pb: ProgressBar,
    }

    let mut evals: Vec<Eval> = Vec::new();
    let mut queue: VecDeque<Shard> = VecDeque::new();
    for &i in &todo {
        let (commit, system) = (&pairs[i].0, &pairs[i].1);
        let names = enumerate_names(repo, commit, system)?;
        let idx = evals.len();
        let mut shards_total = 0;
        let mut s = 0;
        while s < names.len() {
            let e = (s + NAMES_PER_SHARD).min(names.len());
            queue.push_back(Shard {
                eval: idx,
                names: s..e,
            });
            shards_total += 1;
            s = e;
        }
        let short: String = commit.chars().take(12).collect();
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} ⏱ {elapsed} {msg}")
                .expect("valid template"),
        );
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_message(format!("evaluating {short} ({system})"));
        evals.push(Eval {
            commit,
            system,
            label: format!("{short} ({system})"),
            names,
            shards_total,
            shards_done: AtomicUsize::new(0),
            attrs_done: AtomicUsize::new(0),
            rows: Mutex::new(Vec::new()),
            pb,
        });
    }

    struct Q {
        queue: VecDeque<Shard>,
        /// Shards not yet completed (queued or running); requeues don't count
        /// down, so workers only exit when everything truly finished (or on a
        /// fatal error).
        outstanding: usize,
        fatal: Option<anyhow::Error>,
    }
    let q = Mutex::new(Q {
        outstanding: queue.len(),
        queue,
        fatal: None,
    });
    // AIMD over the slot count: halve on an abort (multiplicative decrease),
    // +1 back toward the starting value per few clean shards (additive
    // increase) — titrating concurrency against whatever RAM is really
    // available right now instead of measuring it.
    let target = AtomicUsize::new(slots);
    let successes = AtomicUsize::new(0);

    thread::scope(|s| {
        for w in 0..slots {
            let (q, target, successes, evals, mp) = (&q, &target, &successes, &evals, &mp);
            s.spawn(move || {
                loop {
                    let shard = {
                        let mut g = q.lock().unwrap();
                        if g.fatal.is_some() || g.outstanding == 0 {
                            return;
                        }
                        // Parked slots (w >= target) and an empty-but-not-done
                        // queue both just wait: an aborted shard may requeue.
                        if w < target.load(Ordering::Relaxed) {
                            g.queue.pop_front()
                        } else {
                            None
                        }
                    };
                    let Some(shard) = shard else {
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    };
                    let ev = &evals[shard.eval];
                    let names = &ev.names[shard.names.clone()];

                    let step = || -> Result<()> {
                        let ppath = partial_path(ev.commit, ev.system, names)?;
                        let rows = match crate::evalfile::read_partial(&ppath)? {
                            Some(rows) => rows, // resumed from a prior run
                            None => {
                                let expr = shard_expr(repo, ev.commit, ev.system, names);
                                let rows = stream_jobs(
                                    &expr,
                                    1,
                                    per_worker_mb,
                                    &ev.label,
                                    raw_to_attr_eval,
                                    || {
                                        let n = ev.attrs_done.fetch_add(1, Ordering::Relaxed) + 1;
                                        ev.pb.set_message(format!(
                                            "evaluating {} — {} attrs, {}/{} shards",
                                            ev.label,
                                            n,
                                            ev.shards_done.load(Ordering::Relaxed),
                                            ev.shards_total,
                                        ));
                                    },
                                )?;
                                // Best-effort: a failed persist only costs the
                                // ability to resume this shard later.
                                let _ = crate::evalfile::write_partial(&ppath, &rows);
                                rows
                            }
                        };
                        ev.rows.lock().unwrap().extend(rows);
                        let done = ev.shards_done.fetch_add(1, Ordering::Relaxed) + 1;
                        if done == ev.shards_total {
                            let rows = std::mem::take(&mut *ev.rows.lock().unwrap());
                            write_eval(&eval_path(ev.commit, ev.system)?, &rows)?;
                            if let Some(dir) = ppath.parent() {
                                let _ = fs::remove_dir_all(dir);
                            }
                            ev.pb.finish_with_message(format!(
                                "evaluated {} — {} attrs",
                                ev.label,
                                rows.len()
                            ));
                        }
                        Ok(())
                    };

                    match step() {
                        Ok(()) => {
                            q.lock().unwrap().outstanding -= 1;
                            let n = successes.fetch_add(1, Ordering::Relaxed) + 1;
                            let t = target.load(Ordering::Relaxed);
                            if n % 4 == 0 && t < slots {
                                target.store(t + 1, Ordering::Relaxed);
                            }
                        }
                        Err(e) if e.downcast_ref::<EvalAborted>().is_some() => {
                            let t = target.load(Ordering::Relaxed);
                            let nt = (t / 2).max(1);
                            target.store(nt, Ordering::Relaxed);
                            successes.store(0, Ordering::Relaxed);
                            let _ = mp.println(format!(
                                "  a shard of {} aborted — likely out of memory; \
                                 requeued, slots {t} -> {nt}",
                                ev.label,
                            ));
                            q.lock().unwrap().queue.push_back(shard);
                        }
                        Err(e) => {
                            let mut g = q.lock().unwrap();
                            if g.fatal.is_none() {
                                g.fatal = Some(e);
                            }
                        }
                    }
                }
            });
        }
    });

    match q.into_inner().unwrap().fatal {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Ensure both commits are evaluated across all systems (they run concurrently).
pub fn eval_two(
    repo: &Path,
    base: &str,
    head: &str,
    systems: &[String],
    opts: EvalOpts,
) -> Result<()> {
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(systems.len() * 2);
    for s in systems {
        pairs.push((base.to_string(), s.clone()));
    }
    for s in systems {
        pairs.push((head.to_string(), s.clone()));
    }
    eval_pairs(repo, &pairs, opts)
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
    fn full_expr_fetches_and_imports() {
        let repo = Path::new("/repo");
        let full = build_expr(repo, "abc123", "aarch64-linux");
        assert!(full.contains(r#"builtins.fetchGit { url = "/repo"; rev = "abc123"; }"#));
        assert!(full.contains("allowBroken = true"));
    }

    #[test]
    fn eval_slots_from_invariants() {
        const G: u64 = 1024;
        // Core-bound when RAM is plentiful; RAM-bound (total / worker cap)
        // when it isn't; never zero; --eval-slots wins verbatim (floored at 1).
        assert_eq!(eval_slots(18, 256 * G, 4 * G, None), 18);
        assert_eq!(eval_slots(18, 31 * G, 4 * G, None), 7);
        assert_eq!(eval_slots(4, 2 * G, 4 * G, None), 1);
        assert_eq!(eval_slots(18, 31 * G, 4 * G, Some(3)), 3);
        assert_eq!(eval_slots(18, 31 * G, 4 * G, Some(0)), 1);
    }

    #[test]
    fn shard_expr_subsets_the_import() {
        let e = shard_expr(
            Path::new("/repo"),
            "abc123",
            "aarch64-linux",
            &["hello".into(), "with\"quote".into()],
        );
        // The same import as the whole-set walk, narrowed via listToAttrs,
        // names escaped.
        assert!(e.contains(r#"builtins.fetchGit { url = "/repo"; rev = "abc123"; }"#));
        assert!(e.contains("builtins.listToAttrs"));
        assert!(e.contains(r#""hello" "#));
        assert!(e.contains(r#""with\"quote" "#));
    }
}
