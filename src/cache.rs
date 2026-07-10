//! Substituter (binary cache) facts: is a derivation's output already built and
//! available from `cache.nixos.org`? This is the one remote source npd still
//! consults — a drv-precise, success-only signal (a narinfo either exists or it
//! doesn't). Recorded as `Cache` observations so a later run needn't re-probe.

use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use anyhow::{Context, Result, bail};

const CACHE: &str = "https://cache.nixos.org";

/// How many narinfo probes to run at once. These are independent HTTP HEADs, so
/// the changed set's cache status resolves in one round-trip's worth of time
/// rather than N — the difference between "instant" and "several seconds".
const PROBE_CONCURRENCY: usize = 16;

/// The 32-char store-path hash component of a `/nix/store/<hash>-name` path.
fn store_hash(path: &str) -> Option<&str> {
    path.rsplit('/').next().and_then(|n| n.split('-').next())
}

/// The realised output paths of a derivation, via `nix-store --query
/// --outputs` (fails if the .drv isn't in the local store). The one such
/// helper — the build driver's validity checks use it too.
pub fn drv_outputs(drv: &str) -> Result<Vec<String>> {
    let out = Command::new("nix-store")
        .args(["--query", "--outputs", drv])
        .output()
        .context("running nix-store --query --outputs")?;
    if !out.status.success() {
        bail!("nix-store --query --outputs {drv} failed");
    }
    Ok(lines(&out.stdout))
}

/// Non-empty trimmed lines of a command's output.
pub fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Is this exact output path in the binary cache? (narinfo HEAD -> 2xx.)
fn output_in_cache(out_path: &str) -> bool {
    let Some(hash) = store_hash(out_path) else {
        return false;
    };
    // ureq returns Err for 4xx/5xx and transport errors; only 2xx is Ok.
    ureq::head(&format!("{CACHE}/{hash}.narinfo"))
        .call()
        .is_ok()
}

/// Is any of `drv`'s outputs in the binary cache — i.e. substitutable without a
/// local build? Used by the build driver to avoid "building" (really fetching)
/// a cached path and mislabelling it as a local build. A drv whose outputs
/// can't even be queried probes as not-substitutable — the safe direction: the
/// driver just builds it.
fn in_cache(drv: &str) -> bool {
    drv_outputs(drv)
        .map(|outs| outs.iter().any(|o| output_in_cache(o)))
        .unwrap_or(false)
}

/// Probe several drvs at once, returning `drv -> substitutable?`. A shared cursor
/// hands each of [`PROBE_CONCURRENCY`] worker threads the next drv, so the wall
/// time is `ceil(n / workers)` round-trips rather than `n`.
pub fn in_cache_many(drvs: &[String]) -> HashMap<String, bool> {
    if drvs.is_empty() {
        return HashMap::new();
    }
    let cursor = AtomicUsize::new(0);
    let workers = PROBE_CONCURRENCY.min(drvs.len());
    let results: Vec<(String, bool)> = thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(drv) = drvs.get(i) else { break };
                        local.push((drv.clone(), in_cache(drv)));
                    }
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    results.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_hash_of_output() {
        assert_eq!(
            store_hash("/nix/store/qpp9968dpkv1c755nk13mrkrzpsvah18-hello-2.12.3"),
            Some("qpp9968dpkv1c755nk13mrkrzpsvah18")
        );
    }
}
