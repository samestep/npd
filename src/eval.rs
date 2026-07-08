//! Evaluate a nixpkgs revision into an `attr -> drv` map via `nix-eval-jobs`,
//! cached in the SQLite store. This is the first spine primitive (DESIGN.md §6,
//! §9): a pure fact keyed by `(commit, system, profile)`, computed at most once.
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
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use serde::Deserialize;

use crate::model::{AttrEval, Existence};

/// Bumped when the eval file format or *how* we invoke `nix-eval-jobs` changes in
/// a way that could alter the stored attr->drv map; cache entries under a
/// different version are ignored (and regenerated), never parsed by newer code.
/// v2: drv paths stored stripped of the `/nix/store/` prefix and `.drv` suffix.
/// v3: the (stripped) TSV is zstd-compressed on disk.
pub const EVAL_VERSION: u32 = 3;

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
    drv_path: Option<String>,
    error: Option<String>,
    meta: Option<RawMeta>,
}

fn raw_to_attr_eval(raw: RawJob) -> AttrEval {
    let RawJob {
        attr,
        drv_path,
        error,
        meta,
    } = raw;
    match drv_path {
        Some(drv) => {
            let meta = meta.unwrap_or_default();
            let blocked = matches!(meta.broken, Some(true))
                || matches!(meta.unsupported, Some(true))
                || matches!(meta.insecure, Some(true));
            AttrEval {
                attr,
                existence: if blocked {
                    Existence::Blocked
                } else {
                    Existence::Buildable
                },
                drv_path: Some(drv),
                broken: meta.broken,
                unsupported: meta.unsupported,
                insecure: meta.insecure,
                hydra_platforms_ok: None,
                error: None,
            }
        }
        None => AttrEval {
            attr,
            existence: Existence::Error,
            drv_path: None,
            broken: None,
            unsupported: None,
            insecure: None,
            hydra_platforms_ok: None,
            error,
        },
    }
}

/// Stream NDJSON values off `reader`, mapping each to an `AttrEval`. Memory stays
/// bounded to one value at a time rather than the whole (meta-heavy) output.
fn parse_jobs<R: std::io::Read>(reader: R) -> Result<Vec<AttrEval>> {
    let mut out = Vec::new();
    for item in serde_json::Deserializer::from_reader(reader).into_iter::<RawJob>() {
        let raw = item.context("parsing nix-eval-jobs output")?;
        out.push(raw_to_attr_eval(raw));
    }
    Ok(out)
}

// --- running the evaluator --------------------------------------------------

/// Build the whole-package-set Nix expression `nix-eval-jobs` walks. The
/// revision's source is fetched by `builtins.fetchGit`.
fn build_expr(repo: &Path, commit: &str, system: &str, profile: &str) -> Result<String> {
    let cfg = profile_config(profile)?;
    Ok(format!(
        "import (builtins.fetchGit {{ url = \"{}\"; rev = \"{commit}\"; }}) \
         {{ system = \"{system}\"; config = {cfg}; }}",
        repo.display()
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
    // nix-eval-jobs prints a full Nix traceback per errored attr (megabytes over a
    // whole package set), and the actionable per-attr error is already in the
    // stdout JSON — so we neither inherit its stderr (terminal spam) nor persist
    // it to disk. A thread drains stderr into a bounded ring buffer, keeping only
    // the last few lines for the fatal-error diagnostic below; draining it (vs. an
    // undrained pipe) also can't deadlock while we stream stdout.
    let short: String = commit.chars().take(12).collect();

    // No `--meta`: we only keep `attr → drv`, and `--meta` forces each package's
    // meta attrset (extra evaluation, plus extra allocation that inflates the
    // GC-heavy heap — especially costly on macOS). It doesn't affect drvPaths, so
    // cached eval files stay valid.
    let mut child = Command::new("nix-eval-jobs")
        .args([
            "--workers",
            &workers.to_string(),
            "--max-memory-size",
            &per_worker_mb.to_string(),
            "--expr",
            &expr,
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
    pb.set_message(format!("evaluating {short} ({system}, {workers}w)"));
    let mut attrs = Vec::new();
    for item in serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<RawJob>() {
        attrs.push(raw_to_attr_eval(item.context("parsing nix-eval-jobs output")?));
        pb.set_message(format!("evaluating {short} ({system}, {workers}w) — {} attrs", attrs.len()));
    }
    pb.finish_with_message(format!("evaluated {short} ({system}) — {} attrs", attrs.len()));

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
        bail!(
            "nix-eval-jobs did not finish evaluating {commit} ({system}): it exited \
             {status} after streaming {} attr(s), so the result is truncated and \
             will NOT be cached. A worker most likely died — commonly out-of-memory: \
             reduce the worker count or --max-memory-size so their caps fit in RAM. \
             Last stderr:\n{}",
            attrs.len(),
            stderr_tail,
        );
    }
    Ok(attrs)
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
/// global flag so the scheme can be tuned point-by-point without env vars.
#[derive(Debug, Clone, Copy, Default)]
pub struct EvalOpts {
    pub mem_budget_mb: Option<u64>,
    pub worker_mem_mb: Option<u64>,
    pub concurrency: Option<u64>,
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
    macos_available_mb()
        .or_else(macos_total_mb)
        .unwrap_or(8192)
}

/// macOS available RAM (MiB): free + inactive + speculative + purgeable pages,
/// per `vm_stat`. A heuristic (like Activity Monitor's "available"), but far
/// better than assuming all of `hw.memsize` is free.
fn macos_available_mb() -> Option<u64> {
    let out = Command::new("vm_stat").output().ok().filter(|o| o.status.success())?;
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
    let avail = pages("Pages free") + pages("Pages inactive")
        + pages("Pages speculative") + pages("Pages purgeable");
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
    let budget_mb = opts.mem_budget_mb.unwrap_or_else(|| available_mem_mb() * 8 / 10);
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
// `attr\tdrv` line per attr, sorted by attr (empty drv = no derivation), so the
// diff is a linear two-pointer merge.
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
    Ok(cache_root()?
        .join("evals")
        .join(format!("{commit}-{system}-{profile}-v{EVAL_VERSION}.tsv.zst")))
}

/// Write an eval to its file, sorted by attr, zstd-compressed, atomically (temp +
/// rename) so a crash can never leave a truncated file that would poison the cache.
fn write_eval(path: &Path, attrs: &[AttrEval]) -> Result<()> {
    let mut rows: Vec<(&str, &str)> = attrs
        .iter()
        .map(|a| (a.attr.as_str(), a.drv_path.as_deref().map(strip_drv).unwrap_or("")))
        .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut buf = String::with_capacity(rows.len() * 96);
    for (attr, drv) in rows {
        buf.push_str(attr);
        buf.push('\t');
        buf.push_str(drv);
        buf.push('\n');
    }
    // Level 0 = zstd's default level (currently 3); pass the sentinel rather than
    // a number so we track the library's default rather than pinning it.
    let compressed = zstd::encode_all(buf.as_bytes(), 0).context("compressing eval")?;
    fs::create_dir_all(path.parent().unwrap()).context("creating evals dir")?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &compressed).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).context("renaming eval into place")?;
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
    debug_assert!(stripped.is_some(), "drv not /nix/store/<hash>-<name>.drv: {drv}");
    stripped.unwrap_or(drv)
}

/// Reconstruct a full drv path from its stored (stripped) form — see [`strip_drv`].
fn restore_drv(field: Option<&str>) -> Option<String> {
    field.map(|s| format!("/nix/store/{s}.drv"))
}

/// Parse an eval file's bytes into `(attr, Option<stored-drv>)` pairs, borrowing
/// from `buf` (no per-attr allocation). The drv is left in its stored form (see
/// [`strip_drv`]); since that encoding is injective, the merge can compare stored
/// fields directly and only [`restore_drv`] the few rows it emits. Assumes the
/// file is already sorted by attr.
fn parse_eval(buf: &str) -> Vec<(&str, Option<&str>)> {
    buf.lines()
        .map(|l| {
            let (attr, drv) = l.split_once('\t').unwrap_or((l, ""));
            (attr, if drv.is_empty() { None } else { Some(drv) })
        })
        .collect()
}

/// One changed attr between two evals: its path and its drv on each side (`None`
/// = absent/no derivation there).
pub type ChangedDrv = (String, Option<String>, Option<String>);

/// The changed set between two cached evals — one [`ChangedDrv`] for each attr
/// whose drv differs — via a linear two-pointer merge over the two sorted files.
/// Only the (few) changed rows are allocated.
pub fn changed_set(base: &str, head: &str, system: &str, profile: &str) -> Result<Vec<ChangedDrv>> {
    let bp = eval_path(base, system, profile)?;
    let hp = eval_path(head, system, profile)?;
    let bbuf = read_eval(&bp)?;
    let hbuf = read_eval(&hp)?;
    let b = parse_eval(&bbuf);
    let h = parse_eval(&hbuf);

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < b.len() && j < h.len() {
        match b[i].0.cmp(h[j].0) {
            std::cmp::Ordering::Less => {
                if b[i].1.is_some() {
                    out.push((b[i].0.to_string(), restore_drv(b[i].1), None));
                }
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                if h[j].1.is_some() {
                    out.push((h[j].0.to_string(), None, restore_drv(h[j].1)));
                }
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                if b[i].1 != h[j].1 {
                    out.push((b[i].0.to_string(), restore_drv(b[i].1), restore_drv(h[j].1)));
                }
                i += 1;
                j += 1;
            }
        }
    }
    for k in &b[i..] {
        if k.1.is_some() {
            out.push((k.0.to_string(), restore_drv(k.1), None));
        }
    }
    for k in &h[j..] {
        if k.1.is_some() {
            out.push((k.0.to_string(), None, restore_drv(k.1)));
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
    eprintln!(
        "  eval plan: {} job(s), budget {}MB / {}MB per worker = {} slot(s) \
         -> {} concurrent x {} worker(s)",
        todo.len(),
        plan.budget_mb,
        plan.per_worker_mb,
        plan.slots,
        plan.concurrency,
        plan.workers,
    );
    let sem = Semaphore::new(plan.concurrency);
    let mp = MultiProgress::new();
    let computed: Vec<(usize, Vec<AttrEval>)> =
        thread::scope(|s| -> Result<Vec<(usize, Vec<AttrEval>)>> {
            let mut handles = Vec::new();
            for &i in &todo {
                let (commit, system) = (&pairs[i].0, &pairs[i].1);
                let pb = mp.add(ProgressBar::new_spinner());
                let sem = &sem;
                handles.push(s.spawn(move || -> Result<(usize, Vec<AttrEval>)> {
                    sem.acquire();
                    let r =
                        run_eval_pb(repo, commit, system, profile, plan.workers, plan.per_worker_mb, &pb);
                    sem.release();
                    Ok((i, r?))
                }));
            }
            let mut out = Vec::new();
            for h in handles {
                out.push(h.join().expect("eval thread panicked")?);
            }
            Ok(out)
        })?;
    for (i, attrs) in &computed {
        let (commit, system) = &pairs[*i];
        write_eval(&eval_path(commit, system, profile)?, attrs)?;
    }
    Ok(())
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

    #[test]
    fn parses_success_blocked_and_error_lines() {
        let stdout = concat!(
            r#"{"attr":"hello","attrPath":["hello"],"drvPath":"/nix/store/a-hello.drv","meta":{"broken":false,"unsupported":false},"system":"aarch64-linux"}"#,
            "\n",
            r#"{"attr":"br","drvPath":"/nix/store/b-br.drv","meta":{"broken":true}}"#,
            "\n",
            r#"{"attr":"bad","attrPath":["bad"],"error":"boom","fatal":false}"#,
            "\n",
        );
        let attrs = parse_jobs(stdout.as_bytes()).unwrap();
        assert_eq!(attrs.len(), 3);

        assert_eq!(attrs[0].attr, "hello");
        assert_eq!(attrs[0].existence, Existence::Buildable);
        assert_eq!(attrs[0].drv_path.as_deref(), Some("/nix/store/a-hello.drv"));

        assert_eq!(attrs[1].existence, Existence::Blocked);
        assert_eq!(attrs[1].broken, Some(true));
        assert!(attrs[1].drv_path.is_some());

        assert_eq!(attrs[2].existence, Existence::Error);
        assert_eq!(attrs[2].drv_path, None);
        assert_eq!(attrs[2].error.as_deref(), Some("boom"));
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
        let ae = |attr: &str, drv: Option<&str>| AttrEval {
            attr: attr.into(),
            existence: Existence::Buildable,
            drv_path: drv.map(str::to_string),
            broken: None,
            unsupported: None,
            insecure: None,
            hydra_platforms_ok: None,
            error: None,
        };
        let attrs = [
            ae("hello", Some("/nix/store/a-hello.drv")),
            ae("bad", None),
        ];
        let dir = std::env::temp_dir().join(format!("npd-eval-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("e.tsv");
        write_eval(&path, &attrs).unwrap();

        // On disk the drv is stripped; a no-derivation attr is an empty field
        // (sorted by attr: bad, hello). The file is zstd-compressed, so read it
        // back through the same helper the diff uses.
        let raw = read_eval(&path).unwrap();
        assert_eq!(raw, "bad\t\nhello\ta-hello\n");

        // Parsing + restoring recovers the original drv paths exactly.
        let parsed = parse_eval(&raw);
        let restored: Vec<_> = parsed.iter().map(|(a, d)| (*a, restore_drv(*d))).collect();
        assert_eq!(restored[0], ("bad", None));
        assert_eq!(restored[1], ("hello", Some("/nix/store/a-hello.drv".into())));
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
