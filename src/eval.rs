//! Evaluate a nixpkgs revision into an `attr -> drv` map via `nix-eval-jobs`,
//! cached on disk. This is the first spine primitive (DESIGN.md §6, §9): a pure
//! fact keyed by `(commit, system, profile)`, so it is computed at most once.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::model::{AttrEval, Existence};

/// Bumped if we change *how* we invoke `nix-eval-jobs` in a way that could alter
/// the attr->drv map; old cache entries under a different version are ignored.
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

fn parse_jobs(stdout: &str) -> Result<Vec<AttrEval>> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawJob = serde_json::from_str(line)
            .with_context(|| format!("parsing nix-eval-jobs output line: {line}"))?;
        out.push(raw_to_attr_eval(raw));
    }
    Ok(out)
}

// --- running the evaluator --------------------------------------------------

/// Build the Nix expression `nix-eval-jobs` walks. With no `scope` it is the
/// whole package set at `nixpkgs`; with a scope it is just those (dotted) attrs.
fn build_expr(nixpkgs: &Path, system: &str, profile: &str, scope: &[String]) -> Result<String> {
    let cfg = profile_config(profile)?;
    let base = format!(
        "import {} {{ system = \"{system}\"; config = {cfg}; }}",
        nixpkgs.display()
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

fn run_eval(nixpkgs: &Path, system: &str, profile: &str, scope: &[String]) -> Result<Vec<AttrEval>> {
    let expr = build_expr(nixpkgs, system, profile, scope)?;
    let output = Command::new("nix-eval-jobs")
        .args(["--meta", "--workers", "4", "--expr", &expr])
        .output()
        .context("running nix-eval-jobs (is it on PATH? use the flake dev shell)")?;
    // nix-eval-jobs exits non-zero if any attr errors but still emits JSON for
    // the rest, so parse stdout regardless; only bail if it gave us nothing.
    if !output.status.success() && output.stdout.is_empty() {
        bail!(
            "nix-eval-jobs failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    parse_jobs(&String::from_utf8_lossy(&output.stdout))
}

// --- worktrees and cache ----------------------------------------------------

pub fn cache_root() -> Result<PathBuf> {
    Ok(dirs::cache_dir()
        .context("could not determine cache directory")?
        .join("npd"))
}

/// Add (or reuse) a detached worktree of `repo` at `commit` under the cache.
fn worktree(repo: &Path, commit: &str, cache: &Path) -> Result<PathBuf> {
    let dir = cache.join("worktrees").join(commit);
    if dir.exists() {
        return Ok(dir);
    }
    fs::create_dir_all(dir.parent().unwrap()).context("creating worktrees dir")?;
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach"])
        .arg(&dir)
        .arg(commit)
        .status()
        .context("running git worktree add")?;
    if !status.success() {
        bail!("git worktree add failed for {commit} in {}", repo.display());
    }
    Ok(dir)
}

fn eval_cache_path(cache: &Path, commit: &str, system: &str, profile: &str) -> PathBuf {
    cache.join("evals").join(system).join(format!(
        "{commit}-{profile}-v{EVAL_VERSION}.json"
    ))
}

fn load_cached(path: &Path) -> Option<Vec<AttrEval>> {
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn store_cached(path: &Path, attrs: &[AttrEval]) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap()).context("creating evals dir")?;
    fs::write(path, serde_json::to_string(attrs)?).context("writing eval cache")?;
    Ok(())
}

/// A completed evaluation and where its result came from.
pub struct Eval {
    pub system: String,
    pub attrs: Vec<AttrEval>,
    pub from_cache: bool,
}

/// Evaluate `commit` for each `system`. Full-set evals (`scope` empty) are
/// cached read-through; scoped evals are ad-hoc and always run fresh.
pub fn eval_commit(
    repo: &Path,
    commit: &str,
    systems: &[String],
    profile: &str,
    scope: &[String],
) -> Result<Vec<Eval>> {
    let cache = cache_root()?;
    let wt = worktree(repo, commit, &cache)?;
    let mut results = Vec::new();
    for system in systems {
        let path = eval_cache_path(&cache, commit, system, profile);
        if scope.is_empty()
            && let Some(attrs) = load_cached(&path)
        {
            results.push(Eval {
                system: system.clone(),
                attrs,
                from_cache: true,
            });
            continue;
        }
        let attrs = run_eval(&wt, system, profile, scope)?;
        if scope.is_empty() {
            store_cached(&path, &attrs)?;
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
        let attrs = parse_jobs(stdout).unwrap();
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
    fn full_expr_imports_the_set_scoped_expr_selects() {
        let np = Path::new("/nix/store/np");
        let full = build_expr(np, "aarch64-linux", "default", &[]).unwrap();
        assert!(full.starts_with("import /nix/store/np {"));
        assert!(full.contains("allowBroken = true"));

        let scoped =
            build_expr(np, "aarch64-linux", "default", &["python3Packages.numpy".into()]).unwrap();
        assert!(scoped.contains(r#"attrByPath [ "python3Packages" "numpy" ]"#));
    }

    #[test]
    fn cache_round_trips() {
        let dir = std::env::temp_dir().join(format!("npd-eval-test-{}", std::process::id()));
        let path = dir.join("e.json");
        let attrs = vec![AttrEval {
            attr: "hello".into(),
            existence: Existence::Buildable,
            drv_path: Some("/nix/store/a.drv".into()),
            broken: Some(false),
            unsupported: None,
            insecure: None,
            hydra_platforms_ok: None,
            error: None,
        }];
        store_cached(&path, &attrs).unwrap();
        assert_eq!(load_cached(&path).unwrap(), attrs);
        let _ = fs::remove_dir_all(&dir);
    }
}
