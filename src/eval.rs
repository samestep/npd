//! Evaluate a nixpkgs revision into an `attr -> drv` map via `nix-eval-jobs`,
//! cached in the SQLite store. This is the first spine primitive (DESIGN.md §6,
//! §9): a pure fact keyed by `(commit, system, profile)`, computed at most once.
//!
//! The revision's source comes from `builtins.fetchGit`, so Nix fetches and
//! caches it in the store — npd manages no worktrees. `nix-eval-jobs` output is
//! parsed by streaming NDJSON straight off the child's stdout (never buffering
//! the whole, meta-heavy output).

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indicatif::ProgressBar;
use serde::Deserialize;

use crate::model::{AttrEval, Existence};
use crate::store::Store;

/// Bumped if we change *how* we invoke `nix-eval-jobs` in a way that could alter
/// the attr->drv map; cache entries under a different version are ignored.
pub const EVAL_VERSION: u32 = 1;

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

/// Build the Nix expression `nix-eval-jobs` walks. The revision's source is
/// fetched by `builtins.fetchGit`. With no `scope` it is the whole package set;
/// with a scope it is just those (dotted) attrs.
fn build_expr(
    repo: &Path,
    commit: &str,
    system: &str,
    profile: &str,
    scope: &[String],
) -> Result<String> {
    let cfg = profile_config(profile)?;
    let base = format!(
        "import (builtins.fetchGit {{ url = \"{}\"; rev = \"{commit}\"; }}) \
         {{ system = \"{system}\"; config = {cfg}; }}",
        repo.display()
    );
    if scope.is_empty() {
        return Ok(base);
    }
    let entries: String = scope
        .iter()
        .map(|p| {
            let path_list = p
                .split('.')
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(" ");
            format!("\"{p}\" = pkgs.lib.attrByPath [ {path_list} ] (throw \"missing attr {p}\") pkgs;")
        })
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!("let pkgs = {base}; in {{ {entries} }}"))
}

fn run_eval(
    repo: &Path,
    commit: &str,
    system: &str,
    profile: &str,
    scope: &[String],
) -> Result<Vec<AttrEval>> {
    let expr = build_expr(repo, commit, system, profile, scope)?;
    // Send stderr to a log file rather than inheriting it: nix-eval-jobs prints a
    // full Nix traceback per errored attr (megabytes over a whole package set),
    // and the actionable per-attr error is already in the stdout JSON. A file
    // (unlike an undrained pipe) also can't deadlock while we stream stdout.
    let log_dir = cache_root()?.join("logs");
    fs::create_dir_all(&log_dir).context("creating log dir")?;
    let log_path = log_dir.join("eval.log");
    let log = File::create(&log_path).context("creating eval log")?;

    let mut child = Command::new("nix-eval-jobs")
        .args(["--meta", "--workers", "4", "--expr", &expr])
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .spawn()
        .context("spawning nix-eval-jobs (on PATH? use the flake dev shell)")?;
    let stdout = child.stdout.take().expect("stdout is piped");

    // A full-set eval takes minutes; stream a live attr counter so it never
    // looks hung. Progress goes to stderr (stdout stays clean for piping).
    // A full-set eval takes minutes; a spinner keeps it visibly alive. Like npc,
    // set_message per attr is cheap — the steady tick repaints (to stderr) every
    // 100ms, so this both throttles redraws and shows the true running count.
    let short: String = commit.chars().take(12).collect();
    let spinner = ProgressBar::new_spinner();
    spinner.enable_steady_tick(Duration::from_millis(100));
    spinner.set_message(format!("evaluating {short} ({system})…"));
    let mut attrs = Vec::new();
    for item in serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<RawJob>() {
        attrs.push(raw_to_attr_eval(item.context("parsing nix-eval-jobs output")?));
        spinner.set_message(format!("evaluating {short} ({system})… {} attrs", attrs.len()));
    }
    spinner.finish_with_message(format!("evaluated {short} ({system}): {} attrs", attrs.len()));

    let status = child.wait().context("waiting for nix-eval-jobs")?;
    // nix-eval-jobs exits non-zero if *any* attr errored but still emits the
    // rest, so a non-zero status with parsed attrs is normal (a full nixpkgs
    // eval always has some). Only fail if it produced nothing at all.
    if !status.success() && attrs.is_empty() {
        bail!(
            "nix-eval-jobs failed ({status}) and produced no output. Last stderr from {}:\n{}",
            log_path.display(),
            tail(&log_path, 40),
        );
    }
    Ok(attrs)
}

/// The last `lines` lines of a (possibly large) file; empty if unreadable.
fn tail(path: &Path, lines: usize) -> String {
    match fs::read_to_string(path) {
        Ok(s) => {
            let all: Vec<&str> = s.lines().collect();
            all[all.len().saturating_sub(lines)..].join("\n")
        }
        Err(_) => String::new(),
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

/// A completed evaluation and where its result came from.
pub struct Eval {
    pub system: String,
    pub attrs: Vec<AttrEval>,
    pub from_cache: bool,
}

/// Evaluate `commit` for each `system`. Full-set evals (`scope` empty) are
/// cached read-through in SQLite; scoped evals are ad-hoc and always run fresh.
pub fn eval_commit(
    repo: &Path,
    commit: &str,
    systems: &[String],
    profile: &str,
    scope: &[String],
) -> Result<Vec<Eval>> {
    let mut store = Store::open(&db_path()?)?;
    let now = chrono::Utc::now().timestamp();
    let mut results = Vec::new();
    for system in systems {
        if scope.is_empty()
            && let Some(attrs) = store.load_eval(commit, system, profile, EVAL_VERSION)?
        {
            let short: String = commit.chars().take(12).collect();
            eprintln!("  using cached eval: {short} ({system})");
            results.push(Eval {
                system: system.clone(),
                attrs,
                from_cache: true,
            });
            continue;
        }
        let attrs = run_eval(repo, commit, system, profile, scope)?;
        if scope.is_empty() {
            store.store_eval(commit, system, profile, EVAL_VERSION, now, &attrs)?;
        }
        results.push(Eval {
            system: system.clone(),
            attrs,
            from_cache: false,
        });
    }
    Ok(results)
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
    fn full_expr_fetches_and_imports_scoped_expr_selects() {
        let repo = Path::new("/repo");
        let full = build_expr(repo, "abc123", "aarch64-linux", "default", &[]).unwrap();
        assert!(full.contains(r#"builtins.fetchGit { url = "/repo"; rev = "abc123"; }"#));
        assert!(full.contains("allowBroken = true"));

        let scoped =
            build_expr(repo, "abc123", "aarch64-linux", "default", &["python3Packages.numpy".into()])
                .unwrap();
        assert!(scoped.contains(r#"attrByPath [ "python3Packages" "numpy" ]"#));
    }

    #[test]
    fn tail_returns_last_lines() {
        let dir = std::env::temp_dir().join(format!("npd-tail-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.log");
        fs::write(&path, "l1\nl2\nl3\nl4\n").unwrap();
        assert_eq!(tail(&path, 2), "l3\nl4");
        assert_eq!(tail(&path, 99), "l1\nl2\nl3\nl4");
        assert_eq!(tail(&dir.join("missing"), 5), "");
        let _ = fs::remove_dir_all(&dir);
    }
}
