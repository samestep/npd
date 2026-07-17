//! Run `nix-eval-jobs` and schedule the runs: evaluate a nixpkgs revision into
//! an `attr -> drv` map — the first spine primitive (DESIGN.md §6, §9), a pure
//! fact keyed by `(tree, system)` (the git *tree*, not the commit — see
//! [`crate::model::Rev`]), computed at most once and cached as one flat file per
//! eval (the file format and its diff live in [`crate::evalfile`]).
//!
//! The revision's source comes from `builtins.fetchGit` on its [`Rev::commit`],
//! so Nix fetches and caches it in the store — npd manages no worktrees.
//! `nix-eval-jobs` output is parsed by streaming NDJSON straight off the child's
//! stdout (never buffering the whole, meta-heavy output).

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::evalfile::{eval_path, write_eval};
use crate::live;
use crate::model::{AttrEval, Rev, TestJob};

/// The one nixpkgs config every eval runs under. npd owns the config
/// (DESIGN.md §6), which is what makes the eval cache key just
/// `(tree, system)` — changing this line changes the attr→drv map, so cached
/// evals must be discarded (delete `~/.cache/nix-npd`). The allow-flags are
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

/// Fold `--meta`'s availability bits into npd's single "skipped" bit (its
/// meta-blocked analogue of nixpkgs-review's "skipped"): marked broken *or*
/// unsupported-on-this-system *or* insecure. A missing `meta` (an errored attr
/// carries none) reads as not-skipped. Shared by the full-set walk and the
/// targeted test eval so both classify meta the same way.
fn meta_skipped(meta: &RawMeta) -> bool {
    meta.broken == Some(true) || meta.unsupported == Some(true) || meta.insecure == Some(true)
}

fn raw_to_attr_eval(raw: RawJob) -> AttrEval {
    AttrEval {
        attr: raw.attr,
        drv_path: raw.drv_path,
        skipped: meta_skipped(&raw.meta.unwrap_or_default()),
    }
}

/// Map a `--tests` job to a [`TestJob`]. Label from `attrPath` (unquoted
/// elements) rather than `attr` (which nix-eval-jobs quotes for the dotted
/// package component, e.g. `"python3Packages.requests".tests.foo`): element 0
/// is the package we asked for, and the whole path joined is the clean
/// `<pkg>.tests.<name>` label.
fn raw_to_test_job(raw: RawJob) -> TestJob {
    TestJob {
        pkg_attr: raw.attr_path.first().cloned().unwrap_or_default(),
        test_attr: raw.attr_path.join("."),
        skipped: meta_skipped(&raw.meta.unwrap_or_default()),
        drv_path: raw.drv_path,
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

/// A space-separated Nix string-list body — `"a" "b" ` — each element escaped
/// for a Nix `"..."` literal. Shared by every expression builder that
/// interpolates a list of attr names/paths ([`shard_expr`], [`select_expr`],
/// [`build_tests_expr`]), so the escaping lives in one place.
fn nix_string_list(items: &[String]) -> String {
    items
        .iter()
        .map(|s| format!("\"{}\" ", nix_escape(s)))
        .collect()
}

/// Build the whole-package-set Nix expression `nix-eval-jobs` walks. The
/// revision's source is fetched by `builtins.fetchGit` at `rev` (a commit — real
/// or the synthetic one minted for the working tree; the eval depends only on
/// the tree it resolves to, which is why the cache keys on the tree, not this
/// commit — see [`Rev`]). Interpolants are escaped via [`nix_escape`] (the repo
/// path in particular is user input, `--nixpkgs`).
fn build_expr(repo: &Path, rev: &str, system: &str) -> String {
    format!(
        "import (builtins.fetchGit {{ url = \"{}\"; rev = \"{}\"; }}) \
         {{ system = \"{}\"; config = {EVAL_CONFIG}; }}",
        nix_escape(&repo.display().to_string()),
        nix_escape(rev),
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
    instantiate: bool,
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

    // nix-eval-jobs takes the expression inline (`--expr E`) or as a file-path
    // positional. The `--tests` expression lists every changed package, so on a
    // big changed set an inline `--expr` blows past ARG_MAX (E2BIG on spawn);
    // writing it to a temp file and passing the path works for any size (and the
    // small shard/full-set exprs don't care). The evaluated expression is
    // byte-identical either way — same drvs — so this doesn't affect the cached
    // evals. Kept alive until the child exits (nix-eval-jobs reads it at start).
    let mut expr_file = tempfile::Builder::new()
        .prefix("npd-eval-")
        .suffix(".nix")
        .tempfile()
        .context("creating nix-eval-jobs expr file")?;
    expr_file
        .write_all(expr.as_bytes())
        .and_then(|()| expr_file.flush())
        .context("writing nix-eval-jobs expr file")?;
    let workers_s = workers.to_string();
    let max_s = per_worker_mb.to_string();
    let mut cmd = Command::new("nix-eval-jobs");
    cmd.args([
        "--meta",
        "--workers",
        &workers_s,
        "--max-memory-size",
        &max_s,
    ]);
    // `--no-instantiate` evaluates without writing the `.drv` files. npd only
    // needs the drvPath + outputs (both emitted regardless), so skipping the
    // writes is ~40% faster and avoids instantiating the ~114k attrs it never
    // builds. The small changed set is instantiated on demand before building
    // (see [`instantiate`]), which the build and the narinfo probe both need.
    if !instantiate {
        cmd.arg("--no-instantiate");
    }
    let mut child = scrub_env(&mut cmd)
        .arg(expr_file.path())
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
/// the full-set walk uses (`meta_skipped`) also classifies tests, matching
/// nixpkgs-review's "marked broken and skipped" (which gets the same answer by
/// `tryEval`-ing the outPath under a strict config). `mark` stops at
/// derivations, so it never forces a derivation's internals, and each recursed
/// leaf is wrapped in `tryEval` so one throwing test errors only itself — the
/// per-leaf isolation nix-eval-jobs would otherwise give the untransformed tree.
fn build_tests_expr(repo: &Path, rev: &str, system: &str, attrs: &[String]) -> String {
    let list = nix_string_list(attrs);
    const TEMPLATE: &str = r#"
let
  pkgs = @PKGS@;
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
        .replace("@PKGS@", &build_expr(repo, rev, system))
        .replace("@ATTRS@", &list)
}

/// Evaluate the `passthru.tests` of several `(commit, system, packages)`
/// requests **together**, through one shard scheduler — one shared queue,
/// cross-eval load balancing — after all eval finishes. `nodes` are the `tests`
/// leaves the caller already created per-system as each platform's eval landed
/// (parallel to `requests`; DESIGN §9), which this drives to running/done.
/// Returns the resolved [`TestJob`]s per request, parallel to `requests` (one
/// `<pkg>.tests.<name>` per job). Callers pass only the packages not already
/// cached (see `main`); an empty/all-empty `requests` does no work.
pub fn eval_tests(
    repo: &Path,
    requests: &[(Rev, String, Vec<String>)],
    nodes: Vec<Arc<live::Node>>,
    handle: live::LiveHandle<'_>,
) -> Result<Vec<Vec<TestJob>>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let slots = default_slots(None);
    // Slice every request's packages into ~2×`slots` shards total, so the pool
    // stays full and balances across requests (a nixosTest ≈ a whole NixOS
    // system, so the AIMD backoff earns its keep). ~2×slots keeps the nixpkgs
    // spine re-imported no more than the old one-request-at-a-time eval did.
    let total: usize = requests.iter().map(|(_, _, p)| p.len()).sum();
    let shard_size = total.div_ceil(slots * 2).max(1);

    // The `tests` leaves were created per-system as each platform's eval landed
    // (DESIGN §9), so `nodes` is parallel to `requests`; execution is still one
    // grouped scheduler run, after all eval.
    let labels: Vec<String> = requests
        .iter()
        .map(|(rev, system, _)| format!("tests {} ({system})", rev.display))
        .collect();
    let items: Vec<Vec<String>> = requests.iter().map(|(_, _, p)| p.clone()).collect();
    let meta: Vec<(&Rev, &str)> = requests.iter().map(|(r, s, _)| (r, s.as_str())).collect();
    let results: Vec<Mutex<Vec<TestJob>>> = (0..requests.len())
        .map(|_| Mutex::new(Vec::new()))
        .collect();

    run_shards(
        nodes,
        labels,
        items,
        shard_size,
        slots,
        // The count is streamed test *jobs*, not the package count, so `items`
        // is not a meaningful denominator — show a bare count.
        false,
        handle,
        |gi, label, pkgs, on_item| {
            let (rev, system) = meta[gi];
            let expr = build_tests_expr(repo, &rev.commit, system, pkgs);
            stream_jobs(
                &expr,
                1,
                DEFAULT_WORKER_MEM_MB,
                false,
                label,
                raw_to_test_job,
                || on_item(1),
            )
        },
        |gi, rows| {
            results[gi].lock().unwrap().extend(rows);
            Ok(())
        },
    )?;

    Ok(results
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect())
}

// --- scheduling: one queue of shards (DESIGN §6) ------------------------------

/// Default per-worker heap *cap* — the `--max-memory-size` a worker may reach
/// before nix-eval-jobs restarts it. Kept at nix-eval-jobs' 4 GiB default so a
/// giant subtree (haskellPackages ≈ 3–4 GiB) doesn't trip a restart mid-shard
/// and thrash on re-imports. Distinct from the slot-count budget below.
const DEFAULT_WORKER_MEM_MB: u64 = 4096;

/// RAM budget per slot, used only to *count* the starting slots (see
/// [`eval_slots`]) — not a memory cap. A typical shard's worker holds only
/// ~1–1.5 GiB; just the few giant subtrees spike toward the 4 GiB cap. So
/// counting slots at the cap badly under-parallelizes (a 31 GiB box got 7
/// workers when it had 18 cores); ~2 GiB matches the measured best worker counts
/// across 62/31/16 GiB machines, and AIMD backs off if a run overshoots RAM.
const SLOT_MEM_MB: u64 = 2048;

/// Top-level attr names per shard. Larger shards amortize the per-job nixpkgs
/// import (a few seconds each); smaller ones requeue more cheaply and balance
/// better. Measured across all three RAM sizes, ~800–1600 is a flat best (fewer
/// redundant imports, peak still bounded by the RAM ceiling); the old 400 left
/// 20–30% on the table. Overridable per run with `--shard-size`.
const NAMES_PER_SHARD: usize = 1024;

/// Optional overrides for the eval scheduler; `None` = auto from the machine's
/// invariants (see [`eval_slots`]).
#[derive(Debug, Clone, Copy, Default, clap::Args)]
pub struct EvalOpts {
    /// Concurrent shard evaluations (default: min(cores, total RAM / ~2 GiB)).
    #[arg(long = "eval-slots")]
    pub slots: Option<u64>,
    /// Per-`nix-eval-jobs`-worker heap cap, MiB (default: 4096).
    #[arg(long)]
    pub worker_mem_mb: Option<u64>,
    /// Top-level attr names per full-eval shard (default: 1024). Larger = fewer
    /// redundant nixpkgs imports but coarser load-balancing; the sweet spot
    /// scales with the worker count.
    #[arg(long)]
    pub shard_size: Option<usize>,
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

/// The memory ceiling npd plans slots from: total physical RAM, further capped
/// by any cgroup limit the process runs under (a container, or a systemd
/// `MemoryMax=` scope). Unlike *available* RAM (which the old planner used,
/// and which lies — it moves while a minutes-long eval runs), both are
/// **configured promises about the execution environment**, not measurements
/// of a race: physical RAM via `/proc/meminfo MemTotal` (Linux) or
/// `sysctl hw.memsize` (macOS, where cgroups don't exist and the cap is a
/// no-op); the cgroup ceiling via [`cgroup_mem_limit_mb`]. If an admin edits a
/// limit mid-run, the requeue feedback covers it like any other dynamic
/// effect. Fallback 8 GiB.
fn total_mem_mb() -> u64 {
    let physical = physical_mem_mb();
    match cgroup_mem_limit_mb() {
        Some(limit) => physical.min(limit),
        None => physical,
    }
}

/// The tightest cgroup-v2 memory ceiling over this process's ancestry, in MiB
/// — both `memory.max` (the OOM kill line) and `memory.high` (the reclaim
/// throttle, just as bad for throughput). `None` when unlimited or off-Linux.
fn cgroup_mem_limit_mb() -> Option<u64> {
    let cg = fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = cg.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    let root = Path::new("/sys/fs/cgroup");
    let mut dir = PathBuf::from(format!("/sys/fs/cgroup{rel}"));
    let mut min: Option<u64> = None;
    while dir.starts_with(root) && dir != root {
        for f in ["memory.max", "memory.high"] {
            // The unlimited value is the literal string "max": parse fails, skip.
            if let Ok(s) = fs::read_to_string(dir.join(f))
                && let Ok(bytes) = s.trim().parse::<u64>()
            {
                min = Some(min.map_or(bytes, |m| m.min(bytes)));
            }
        }
        if !dir.pop() {
            break;
        }
    }
    min.map(|b| b / 1024 / 1024)
}

fn physical_mem_mb() -> u64 {
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
/// worker per slot, so cores, and total RAM divided by a per-slot budget
/// ([`SLOT_MEM_MB`], ~2 GiB — the *typical* worker footprint, deliberately below
/// the 4 GiB restart cap since only the few giant subtrees approach it). The
/// dynamic part of RAM is handled by feedback, not planning: the queue sheds
/// slots when a shard is OOM-killed ([`eval_pairs`]).
fn eval_slots(cores: usize, mem_mb: u64, per_slot_mb: u64, user: Option<u64>) -> usize {
    match user {
        Some(s) => (s as usize).max(1),
        None => cores.min((mem_mb / per_slot_mb.max(1)).max(1) as usize),
    }
}

/// [`eval_slots`] wired to this machine's invariants — the starting slot count
/// every scheduler run uses (the user's `--eval-slots` when set, else auto).
/// `eval_slots` stays a standalone pure fn so its unit test can pin the arithmetic.
fn default_slots(user: Option<u64>) -> usize {
    let cores = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    eval_slots(cores, total_mem_mb(), SLOT_MEM_MB, user)
}

/// What a phase's commit leaves show in the number column: nothing (a state
/// color only, e.g. `enumerate`, which has no meaningful count to tick), a plain
/// count (`tests`; or `count / total` when [`run_shards`] is told the total, e.g.
/// `instantiate`), or a dim `NN%` shard-progress readout (`evaluate`).
#[derive(Clone, Copy)]
enum Leaf {
    None,
    Count,
    Percent,
}

/// Build a phase subtree in `tree`: the phase node, then (for a multi-system
/// run) a system level, then the per-side commit `display` leaves — returning
/// the leaf handles in `groups` order (parallel to the scheduler's groups, one
/// `(system, display)` each). The single-system run elides the system level, so
/// the commits sit directly under the phase (DESIGN §6). `leaf` picks the leaves'
/// number-column kind.
fn add_phase(
    tree: &live::Tree,
    phase: &str,
    groups: &[(String, String)],
    leaf: Leaf,
) -> Vec<Arc<live::Node>> {
    tree.node(phase, 0);
    let make = |disp: String, depth: usize| match leaf {
        Leaf::None => tree.node(disp, depth),
        Leaf::Count => tree.counter(disp, depth, -1),
        Leaf::Percent => tree.percent(disp, depth),
    };
    let mut handles: Vec<Option<Arc<live::Node>>> = vec![None; groups.len()];
    if tree.multi() {
        // Distinct systems in first-seen order; each side's commit nests under it.
        let mut order: Vec<&str> = Vec::new();
        for (s, _) in groups {
            if !order.contains(&s.as_str()) {
                order.push(s);
            }
        }
        for s in order {
            tree.node(s.to_string(), 1);
            for (gi, (gs, disp)) in groups.iter().enumerate() {
                if gs == s {
                    handles[gi] = Some(make(disp.clone(), 2));
                }
            }
        }
    } else {
        for (gi, (_s, disp)) in groups.iter().enumerate() {
            handles[gi] = Some(make(disp.clone(), 1));
        }
    }
    handles.into_iter().map(Option::unwrap).collect()
}

/// The top-level attr names of the package set at `(commit, system)` — the
/// space the shards partition. Cheap (well under a second warm): forcing
/// `attrNames` touches no derivations. The literal `recurseForDerivations`
/// key is dropped to mirror how `nix-eval-jobs` skips it when walking a set.
fn enumerate_names(repo: &Path, rev: &str, system: &str) -> Result<Vec<String>> {
    let expr = format!("builtins.attrNames ({})", build_expr(repo, rev, system));
    let out = scrub_env(
        Command::new("nix-instantiate").args(["--eval", "--strict", "--json", "-E", &expr]),
    )
    .output()
    .context("running nix-instantiate (attr names)")?;
    if !out.status.success() {
        bail!(
            "enumerating top-level attrs of {rev} ({system}) failed:\n{}",
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
fn shard_expr(repo: &Path, rev: &str, system: &str, names: &[String]) -> String {
    let list = nix_string_list(names);
    format!(
        "let pkgs = {}; in builtins.listToAttrs \
         (map (n: {{ name = n; value = pkgs.${{n}}; }}) [ {list}])",
        build_expr(repo, rev, system)
    )
}

/// A job expression selecting exactly `paths` out of the package set — each an
/// attr path, possibly dotted/nested (`python3Packages.foo`, or a test path like
/// `grafana.tests.grafana.basic`). One job per path, forced per-attr in the
/// worker, so a path that no longer resolves errors only itself.
///
/// TODO(nix-eval-jobs#412): this hand-rolled selector — and the identical
/// `splitString "."` trick in [`build_tests_expr`] — is the wrapper-expr
/// workaround that a native `--select <attrpath>` (emitting the literal selector
/// as `attr`) would replace. `splitString "."` also mis-splits a quoted path
/// element like `haskell.compiler."ghc94"`, which `--select` would handle
/// correctly. Adopt it (in both spots) once it lands upstream.
fn select_expr(repo: &Path, rev: &str, system: &str, paths: &[String]) -> String {
    let list = nix_string_list(paths);
    format!(
        "let pkgs = {}; lib = pkgs.lib; in builtins.listToAttrs \
         (map (p: {{ name = p; value = lib.attrByPath (lib.splitString \".\" p) null pkgs; }}) [ {list}])",
        build_expr(repo, rev, system)
    )
}

/// Write the changed set's `.drv` files to the store. npd's evals run with
/// `--no-instantiate` (drvPath + outputs only — no `.drv` writes for the ~114k
/// attrs it never builds), so the drvs the build and the narinfo probe actually
/// touch — the small changed set — are materialized here, one `nix-eval-jobs`
/// run per `(commit, system)` with instantiation on. Streamed rows are
/// discarded; the store write is the point. Runs only when about to build.
///
/// The per-pair runs go through the **same shard scheduler** as the two eval
/// paths (`run_shards`) so they run concurrently and get the identical live
/// display — a fresh multi-system run would otherwise sit silent through six
/// serial nixpkgs re-imports (base+head × the systems). Each request is **one
/// shard** (no sub-slicing): the whole cost here is the per-run nixpkgs import,
/// so splitting a request's handful of changed attrs across shards would only
/// re-pay that import for no gain. Concurrency is what wins — the phase's
/// wall-time drops from the *sum* of the imports to (roughly) the *slowest*
/// one, up to `slots` at a time, at no extra total work.
pub fn instantiate(
    repo: &Path,
    requests: &[(Rev, String, Vec<String>)],
    tree: &live::Tree,
    handle: live::LiveHandle<'_>,
) -> Result<()> {
    // Drop the sides with nothing to instantiate (a diff side can have no
    // buildable changed attrs) so they don't clutter the display.
    let requests: Vec<&(Rev, String, Vec<String>)> =
        requests.iter().filter(|(_, _, p)| !p.is_empty()).collect();
    if requests.is_empty() {
        return Ok(());
    }
    let slots = default_slots(None);

    let groups: Vec<(String, String)> = requests
        .iter()
        .map(|(rev, sys, _)| (sys.clone(), rev.display.clone()))
        .collect();
    let nodes = add_phase(tree, "instantiate", &groups, Leaf::Count);
    let labels: Vec<String> = requests
        .iter()
        .map(|(rev, system, _)| format!("{} {system}", rev.display))
        .collect();
    let items: Vec<Vec<String>> = requests.iter().map(|(_, _, p)| p.clone()).collect();
    let meta: Vec<(&Rev, &str)> = requests.iter().map(|(r, s, _)| (r, s.as_str())).collect();
    // A shard per request: sizing at the largest request makes every group
    // exactly one shard (no split), so each pair re-imports nixpkgs just once.
    let shard_size = items.iter().map(Vec::len).max().unwrap_or(1).max(1);

    run_shards(
        nodes,
        labels,
        items,
        shard_size,
        slots,
        // One streamed job per requested drv, so `items` is the total.
        true,
        handle,
        |gi, label, paths, on_item| {
            let (rev, system) = meta[gi];
            let expr = select_expr(repo, &rev.commit, system, paths);
            // Streamed rows are discarded (mapped to `()`); the `.drv` writes are
            // the point. The per-job callback drives the live count.
            stream_jobs(
                &expr,
                1,
                DEFAULT_WORKER_MEM_MB,
                true,
                label,
                |_| (),
                || on_item(1),
            )
        },
        |_, _| Ok(()),
    )
}

// --- the shard scheduler (shared by the full-set eval and the --tests eval) ---

/// One group of shards run together: one leaf node in the progress tree, one
/// assembled result. Its `items` (top-level names for the full eval, changed
/// packages for `--tests`) are sliced into shards; the shard counters drive the
/// AIMD scheduler, while progress is reflected onto `node` for the display.
struct ShardGroup<T> {
    node: Arc<live::Node>,
    items: Vec<String>,
    shards_total: usize,
    shards_done: AtomicUsize,
    rows: Mutex<Vec<T>>,
}

/// A queued unit of work: a slice of one group's items.
struct Shard {
    group: usize,
    items: std::ops::Range<usize>,
}

/// Run a set of shard groups through one bounded, AIMD-controlled worker pool,
/// reflecting progress onto each group's [`live::Node`] in the shared tree
/// (DESIGN §6). Shared by the full-set, tests, and instantiate paths. `nodes`
/// gives the leaf node per group (parallel to `items`); `labels` names each
/// group for error messages; `known_total` sets the node's denominator to
/// `items.len()` — true when one streamed row == one item (evaluate,
/// instantiate), false for enumerate (which discovers its count) and tests
/// (whose count is streamed jobs, not packages). Persistence is the caller's job
/// via the closures (the full eval assembles a flat file, `--tests` returns
/// rows; DESIGN §4); this owns only the scheduling and the node updates. The
/// outer `with_live` in `run` owns the refresher that redraws the tree.
///
/// `eval_shard(group, label, items, on_item)` evaluates one shard's item slice
/// to its rows, calling `on_item(n)` as items surface (bumps the node count); it
/// may return an [`EvalAborted`] error to have the shard requeued at reduced
/// concurrency (a note is emitted above the tree via `handle`), or any other
/// error to fail the whole run. `on_group_complete` fires the moment a group's
/// last shard lands, with the group's assembled rows.
#[allow(clippy::too_many_arguments)]
fn run_shards<T: Send>(
    nodes: Vec<Arc<live::Node>>,
    labels: Vec<String>,
    items: Vec<Vec<String>>,
    shard_size: usize,
    slots: usize,
    known_total: bool,
    handle: live::LiveHandle<'_>,
    eval_shard: impl Fn(usize, &str, &[String], &(dyn Fn(usize) + Sync)) -> Result<Vec<T>> + Sync,
    on_group_complete: impl Fn(usize, Vec<T>) -> Result<()> + Sync,
) -> Result<()> {
    let shard_size = shard_size.max(1);
    let groups: Vec<ShardGroup<T>> = nodes
        .into_iter()
        .zip(items)
        .map(|(node, items)| {
            if known_total {
                node.set_total(items.len() as i64);
            }
            let shards_total = items.len().div_ceil(shard_size);
            // Set the shard denominator up front so a percent node's `NN%` is
            // correct from the first frame (not 100% until the first shard lands).
            node.set_shards_total(shards_total);
            ShardGroup {
                node,
                shards_total,
                items,
                shards_done: AtomicUsize::new(0),
                rows: Mutex::new(Vec::new()),
            }
        })
        .collect();

    let mut queue: VecDeque<Shard> = VecDeque::new();
    for (gi, g) in groups.iter().enumerate() {
        let mut s = 0;
        while s < g.items.len() {
            let e = (s + shard_size).min(g.items.len());
            queue.push_back(Shard {
                group: gi,
                items: s..e,
            });
            s = e;
        }
    }
    if queue.is_empty() {
        return Ok(());
    }
    // No point in more workers than shards.
    let slots = slots.min(queue.len());

    struct Q {
        queue: VecDeque<Shard>,
        /// Shards not yet completed (queued or running); requeues don't count
        /// down, so workers only exit when everything truly finished (or fatal).
        outstanding: usize,
        fatal: Option<anyhow::Error>,
    }
    let q = Mutex::new(Q {
        outstanding: queue.len(),
        queue,
        fatal: None,
    });
    // AIMD over the slot count: halve on an abort (multiplicative decrease), +1
    // back toward the starting value per few clean shards (additive increase).
    let target = AtomicUsize::new(slots);
    let successes = AtomicUsize::new(0);

    // The worker pool. Workers only bump atomics / node state and, on a requeue,
    // emit a note above the tree via `handle`; the refresher (owned by the outer
    // `with_live` in `run`) redraws the tree off the node atomics every 100 ms.
    thread::scope(|s| {
        for w in 0..slots {
            let (q, target, successes, groups, labels, eval_shard, on_group_complete) = (
                &q,
                &target,
                &successes,
                &groups,
                &labels,
                &eval_shard,
                &on_group_complete,
            );
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
                    let g = &groups[shard.group];
                    let slice = &g.items[shard.items.clone()];

                    g.node.set_running();
                    g.node.shard_started();
                    let outcome = (|| -> Result<()> {
                        let on_item = |n: usize| g.node.stream(n as i64);
                        let rows = eval_shard(shard.group, &labels[shard.group], slice, &on_item)?;
                        g.rows.lock().unwrap().extend(rows);
                        let done = g.shards_done.fetch_add(1, Ordering::Relaxed) + 1;
                        // Advance a percent node's shard-progress readout (a no-op
                        // for count-less / plain-count nodes).
                        g.node.shard_progress(done);
                        if done == g.shards_total {
                            let rows = std::mem::take(&mut *g.rows.lock().unwrap());
                            let n = rows.len() as i64;
                            on_group_complete(shard.group, rows)?;
                            // Pin a plain count to the assembled total (the streamed
                            // tally can drift), then mark the group done.
                            g.node.group_done(n);
                        }
                        Ok(())
                    })();
                    g.node.shard_finished();

                    match outcome {
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
                            handle.note(&format!(
                                "  a shard of {} aborted — likely out of memory; \
                                 requeued, slots {t} -> {nt}",
                                labels[shard.group],
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

/// Ensure every `(commit, system)` pair has a cached eval file, via **one
/// global queue of shards** (DESIGN §6): each eval's top-level names are
/// split into [`NAMES_PER_SHARD`] slices, and every shard is an independent
/// one-worker `nix-eval-jobs` job. [`eval_slots`] jobs run at once; a shard
/// that aborts (in practice a worker OOM-kill) is simply requeued while the
/// slot count backs off multiplicatively — and creeps back up on sustained
/// success (AIMD). An aborted shard requeues in memory (completed shards' rows
/// are held there too), so an interrupted eval re-runs from scratch rather than
/// resuming; when an eval's last shard lands, its rows are assembled and
/// written as the one cached file.
pub fn eval_pairs(
    repo: &Path,
    pairs: &[(Rev, String)],
    opts: EvalOpts,
    tree: &live::Tree,
    handle: live::LiveHandle<'_>,
    // Called with a `system` the moment one of its eval files lands, so the
    // caller can compute that system's diff and show its `tests` early (DESIGN §9).
    on_eval_done: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    let mut todo: Vec<usize> = Vec::new();
    // Systems with a cache hit this run — signalled to `on_eval_done` once the
    // eval nodes exist (below), so their `tests` can appear early while the cold
    // systems evaluate, yet still sort under `evaluate` (DESIGN §9).
    let mut cached: Vec<&str> = Vec::new();
    // Dedupe on the eval key `(tree, system)`: `npd X X`, repeated --system, or
    // two revisions sharing a tree would otherwise run the same eval twice
    // concurrently — harmless (the write is atomic) but 2× the work.
    let mut seen = std::collections::HashSet::new();
    for (i, (rev, system)) in pairs.iter().enumerate() {
        let path = eval_path(&rev.tree, system)?;
        if path.exists() {
            // A cache hit: mark the file used now so LRU eviction (`--clean`,
            // DESIGN.md §4) keeps a frequently-reused eval (e.g. a shared base)
            // warm rather than judging it by its first-write time.
            crate::evalfile::touch_eval(&path);
            cached.push(system);
        } else if seen.insert((&rev.tree, system)) {
            todo.push(i);
        }
    }
    if todo.is_empty() {
        // Nothing to evaluate — the caller sweeps the (fully-cached) systems
        // itself; we've created no nodes and fire nothing.
        return Ok(());
    }

    let per_worker_mb = opts.worker_mem_mb.unwrap_or(DEFAULT_WORKER_MEM_MB);
    // Count slots at the ~2 GiB typical footprint, but each worker keeps the
    // 4 GiB restart cap (`per_worker_mb`) — the two are deliberately decoupled.
    let slots = default_slots(opts.slots);

    // One shard group per `(tree, system)` for both phases below; `meta` keeps
    // the identifying `(rev, system)` per group (the rev supplies fetchGit's
    // commit; the tree is its cache key), `labels` the error-message name.
    let meta: Vec<(&Rev, &str)> = todo
        .iter()
        .map(|&i| (&pairs[i].0, pairs[i].1.as_str()))
        .collect();
    let labels: Vec<String> = meta
        .iter()
        .map(|(rev, system)| format!("{} {system}", rev.display))
        .collect();
    let groups: Vec<(String, String)> = meta
        .iter()
        .map(|(rev, system)| (system.to_string(), rev.display.clone()))
        .collect();
    // Both phase subtrees are created up front, so `evaluate` shows as waiting
    // (blue) under the same commit displays while `enumerate` runs (DESIGN §6).
    let enum_nodes = add_phase(tree, "enumerate", &groups, Leaf::None);
    let eval_nodes = add_phase(tree, "evaluate", &groups, Leaf::Percent);

    // Now that the eval nodes exist, signal systems already cached this run so
    // their `tests` appear immediately (a side whose other side is still cold is
    // a no-op until that lands — the caller re-checks both files). A cold group
    // signals when it completes (below).
    for system in cached {
        on_eval_done(system);
    }

    // Phase 1: enumerate each pair's top-level attr names — the space phase 2
    // shards. This runs through the *same scheduler*, one shard per pair (the
    // work is a single `builtins.attrNames` call — not a fannable set — so there
    // is nothing to sub-slice): the pairs enumerate concurrently behind the
    // shared live display instead of a bespoke pool. Enumerating a cold commit
    // reads and hashes its whole source tree (a few seconds — even on Nix ≥2.35,
    // where the tree is no longer *copied* into the store, the content-addressed
    // hash still forces a full read); running the pairs concurrently overlaps
    // those hashes instead of summing them (each distinct commit is independent —
    // measured ~2×; Nix's fetcher locks serialize same-commit races, so a warm
    // pair still returns cheaply). Reusing the eval slot count can't oversubscribe
    // RAM — a lone `nix-instantiate` is ~0.5 GB, well under a shard worker's cap.
    let enumerated: Vec<Mutex<Vec<String>>> =
        (0..meta.len()).map(|_| Mutex::new(Vec::new())).collect();
    run_shards(
        enum_nodes,
        labels.clone(),
        // One placeholder item per group ⇒ exactly one shard per pair.
        meta.iter().map(|_| vec![String::new()]).collect(),
        1,
        slots,
        // Enumerate discovers its attr count, so there is no denominator to show.
        false,
        handle,
        |gi, _label, _slice, on_item| {
            let (rev, system) = meta[gi];
            let names = enumerate_names(repo, &rev.commit, system)?;
            on_item(names.len());
            Ok(names)
        },
        |gi, names| {
            *enumerated[gi].lock().unwrap() = names;
            Ok(())
        },
    )?;
    let items: Vec<Vec<String>> = enumerated
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect();

    // Phase 2: shard-evaluate every pair's enumerated names into its cached file.
    run_shards(
        eval_nodes,
        labels,
        items,
        opts.shard_size.unwrap_or(NAMES_PER_SHARD),
        slots,
        // No denominator: `nix-eval-jobs` descends into `recurseForDerivations`
        // sets (haskellPackages, the python sets, …), so it streams far more drvs
        // than there are enumerated top-level names — the enumerated count is not
        // a valid total. Like `enumerate`, show a bare climbing count.
        false,
        handle,
        // Evaluate one shard by streaming its own one-worker `nix-eval-jobs`.
        |gi, label, names, on_item| {
            let (rev, system) = meta[gi];
            let expr = shard_expr(repo, &rev.commit, system, names);
            stream_jobs(
                &expr,
                1,
                per_worker_mb,
                false,
                label,
                raw_to_attr_eval,
                || on_item(1),
            )
        },
        // Assemble the eval into its one cached file, keyed on the tree, then
        // signal that this `(tree, system)` is now available — the last side of a
        // system to land lets the caller diff it and reveal its `tests` (§9).
        |gi, rows| {
            let (rev, system) = meta[gi];
            write_eval(&eval_path(&rev.tree, system)?, &rows)?;
            on_eval_done(system);
            Ok(())
        },
    )
}

/// Ensure both revisions are evaluated across all systems (they run
/// concurrently). Deduped by their eval key `(tree, system)` in [`eval_pairs`],
/// so a `base`/`head` that share a tree pay for one eval, not two.
#[allow(clippy::too_many_arguments)]
pub fn eval_two(
    repo: &Path,
    base: &Rev,
    head: &Rev,
    systems: &[String],
    opts: EvalOpts,
    tree: &live::Tree,
    handle: live::LiveHandle<'_>,
    on_eval_done: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    // System-major, base-then-head — the order the tree displays them (grouped
    // by system, base above head), so the shard scheduler works through them in
    // that same order rather than all bases first (DESIGN §6).
    let mut pairs: Vec<(Rev, String)> = Vec::with_capacity(systems.len() * 2);
    for s in systems {
        pairs.push((base.clone(), s.clone()));
        pairs.push((head.clone(), s.clone()));
    }
    eval_pairs(repo, &pairs, opts, tree, handle, on_eval_done)
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
    fn parses_success_skipped_and_error_lines() {
        // Any of meta.broken/unsupported/insecure folds into the one `skipped`
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
        assert!(!attrs[0].skipped);

        assert!(attrs[1].skipped);
        assert!(attrs[1].drv_path.is_some());
        assert!(attrs[2].skipped);

        assert_eq!(attrs[3].attr, "bad");
        assert_eq!(attrs[3].drv_path, None);
        assert!(!attrs[3].skipped);
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
        // Core-bound when RAM is plentiful; RAM-bound (total / per-slot budget)
        // when it isn't; never zero; --eval-slots wins verbatim (floored at 1).
        // At the default SLOT_MEM_MB (~2 GiB) the three benchmark boxes get:
        assert_eq!(eval_slots(32, 62 * G, SLOT_MEM_MB, None), 31); // amd64 (core-bound near 32)
        assert_eq!(eval_slots(18, 31 * G, SLOT_MEM_MB, None), 15); // aarch64-linux
        assert_eq!(eval_slots(18, 16 * G, SLOT_MEM_MB, None), 8); //  darwin
        assert_eq!(eval_slots(18, 256 * G, SLOT_MEM_MB, None), 18); // core-bound
        assert_eq!(eval_slots(4, 2 * G, SLOT_MEM_MB, None), 1); //    never zero
        assert_eq!(eval_slots(18, 31 * G, SLOT_MEM_MB, Some(3)), 3); // --eval-slots wins
        assert_eq!(eval_slots(18, 31 * G, SLOT_MEM_MB, Some(0)), 1);
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
