//! `--clean`: bound the on-disk cache by evicting eval files (DESIGN.md §4).
//!
//! The eval files (`<system>/<tree>.tsv.zst`) are ~98% of everything npd stores,
//! and each is a standalone, re-derivable artifact keyed on `(tree, system)` — so
//! the cache is bounded by dropping whole eval files, no monolith to vacuum. The
//! last-*used* time is the file's mtime, which `eval::eval_pairs` re-stamps on
//! every cache hit (`evalfile::touch_eval`) so a reused base eval stays warm.
//!
//! Evicting an eval file also purges that `(tree, system)`'s `--tests` rows
//! (`store::Store::purge_tests`): the tests cache is keyed on the same tree, so
//! the two travel together and the DB stays proportional to the eval corpus.
//! The append-only observation log is left untouched — it's keyed on drvpath (no
//! tree to evict by), tiny, and the one thing expensive to re-derive (it
//! remembers *failures*, DESIGN.md §5).

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use crate::paths::{cache_root, db_path};
use crate::store::Store;

/// What `--clean` reduces the eval cache to.
#[derive(Debug, PartialEq, Eq)]
pub enum CleanSpec {
    /// Keep the most-recently-used eval files whose combined size fits in this
    /// many bytes; evict the least-recently-used rest.
    Budget(u64),
    /// Evict every eval file last used strictly before this Unix time (seconds)
    /// — how both a `DATE` and a `DURATION`-ago are expressed.
    Before(u64),
}

impl CleanSpec {
    /// Parse a `--clean` argument: a size budget (`4GiB`, `500MB`, `1048576`), an
    /// absolute date (`2026-07-15`, UTC midnight), or a duration-ago (`2mo`,
    /// `1yr`, `30d`). The three are disjoint — only a date carries `-`, only a
    /// size ends in `B` (or is bare digits), only a duration ends in a time unit.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if let Some(secs) = parse_date(s) {
            return Ok(CleanSpec::Before(secs));
        }
        if let Some(bytes) = parse_size(s) {
            return Ok(CleanSpec::Budget(bytes));
        }
        if let Some(ago) = parse_duration_secs(s) {
            let now = now_secs();
            return Ok(CleanSpec::Before(now.saturating_sub(ago)));
        }
        bail!(
            "could not parse --clean {s:?}: expected a size (4GiB, 500MB), \
             a date (2026-07-15), or a duration (2mo, 1yr, 30d)"
        );
    }
}

/// Split a `<number><unit>` string into its numeric prefix and alphabetic unit.
/// Returns `None` if the number doesn't parse.
fn split_num_unit(s: &str) -> Option<(f64, &str)> {
    let split = s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    Some((num.trim().parse().ok()?, unit))
}

/// A byte count: `4GiB`, `500MB`, `1.5GB`, or bare digits (bytes). Decimal units
/// are powers of 1000, `i`-units powers of 1024.
fn parse_size(s: &str) -> Option<u64> {
    let (num, unit) = split_num_unit(s)?;
    let mult: f64 = match unit {
        "" | "B" => 1.0,
        "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024f64.powi(2),
        "GiB" => 1024f64.powi(3),
        "TiB" => 1024f64.powi(4),
        _ => return None,
    };
    (num >= 0.0).then_some((num * mult) as u64)
}

/// A duration in seconds: `30d`, `2mo`, `1yr`. Months are 30 days, years 365
/// (a cache cutoff needs no calendar precision).
fn parse_duration_secs(s: &str) -> Option<u64> {
    let (num, unit) = split_num_unit(s)?;
    let mult: f64 = match unit {
        "h" => 3600.0,
        "d" => 86_400.0,
        "w" => 604_800.0,
        "mo" => 2_592_000.0,        // 30 days
        "y" | "yr" => 31_536_000.0, // 365 days
        _ => return None,
    };
    (num >= 0.0).then_some((num * mult) as u64)
}

/// A `YYYY-MM-DD` date as its UTC-midnight Unix time (seconds), or `None` if it
/// isn't that shape.
fn parse_date(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    (days >= 0).then_some((days as u64) * 86_400)
}

/// Days since 1970-01-01 for a proleptic-Gregorian `y-m-d` (Howard Hinnant's
/// `days_from_civil`), so a date cutoff needs no calendar crate.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One eval file on disk, with the `(tree, system)` it caches and the size/mtime
/// eviction reads.
struct Eval {
    path: PathBuf,
    tree: String,
    system: String,
    size: u64,
    mtime: u64,
}

/// The indices of `files` to evict under `spec`. Pure (no I/O) so the LRU logic
/// is unit-tested directly:
/// - `Budget`: keep files newest-first while they fit, then evict that file and
///   every older one — strict LRU (a big file that doesn't fit evicts the tail,
///   it doesn't get skipped over to keep something older).
/// - `Before`: evict every file last used before the cutoff.
fn victims(files: &[Eval], spec: &CleanSpec) -> Vec<usize> {
    match spec {
        CleanSpec::Before(cutoff) => (0..files.len())
            .filter(|&i| files[i].mtime < *cutoff)
            .collect(),
        CleanSpec::Budget(budget) => {
            let mut idx: Vec<usize> = (0..files.len()).collect();
            // Newest first; ties broken by size (evict the larger later) then
            // index, so the order is deterministic.
            idx.sort_by(|&a, &b| {
                files[b]
                    .mtime
                    .cmp(&files[a].mtime)
                    .then(files[a].size.cmp(&files[b].size))
                    .then(a.cmp(&b))
            });
            let mut kept = 0u64;
            let mut evicting = false;
            let mut out = Vec::new();
            for i in idx {
                if !evicting && kept + files[i].size <= *budget {
                    kept += files[i].size;
                } else {
                    evicting = true;
                    out.push(i);
                }
            }
            out
        }
    }
}

/// Enumerate every `<system>/<tree>.tsv.zst` eval file under the cache root.
fn gather(root: &std::path::Path) -> Result<Vec<Eval>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for sysent in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let sysent = sysent?;
        // System subdirs only — skip `npd.sqlite` and its sidecars.
        if !sysent.file_type()?.is_dir() {
            continue;
        }
        let system = sysent.file_name().to_string_lossy().into_owned();
        for f in fs::read_dir(sysent.path())? {
            let f = f?;
            let name = f.file_name().to_string_lossy().into_owned();
            let Some(tree) = name.strip_suffix(".tsv.zst") else {
                continue;
            };
            let md = f.metadata()?;
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(Eval {
                path: f.path(),
                tree: tree.to_string(),
                system: system.clone(),
                size: md.len(),
                mtime,
            });
        }
    }
    Ok(out)
}

/// Evict eval files per `spec`, purge each evicted `(tree, system)`'s `--tests`
/// rows, and vacuum the DB once. This is the whole `--clean` action — it reviews
/// nothing. It first prints exactly what it *would* remove and asks for
/// confirmation on stdin, deleting only on a yes (`assume_yes` — the `-y` flag —
/// skips the prompt, for scripts). Nothing is touched until confirmed.
pub fn clean(spec: &CleanSpec, assume_yes: bool) -> Result<()> {
    let root = cache_root()?;
    let files = gather(&root)?;
    let total: u64 = files.iter().map(|f| f.size).sum();
    let victims = victims(&files, spec);

    if victims.is_empty() {
        println!(
            "nothing to evict: {} in {} eval file(s) already within budget",
            human_bytes(total),
            files.len()
        );
        return Ok(());
    }

    // Show what would be removed, oldest-used first (the order eviction favours).
    let freed: u64 = victims.iter().map(|&i| files[i].size).sum();
    let mut shown = victims.clone();
    shown.sort_by_key(|&i| files[i].mtime);
    println!(
        "Would evict {} of {} eval file(s), freeing {} ({} would remain). \
         Each also drops that eval's --tests cache rows:",
        victims.len(),
        files.len(),
        human_bytes(freed),
        human_bytes(total - freed),
    );
    for &i in &shown {
        let f = &files[i];
        println!(
            "  {}/{}  {:>9}  last used {}",
            f.system,
            short_tree(&f.tree),
            human_bytes(f.size),
            fmt_date(f.mtime),
        );
    }

    if !assume_yes && !confirm("Delete these? [y/N] ")? {
        println!("Aborted; nothing deleted.");
        return Ok(());
    }

    let mut store = Store::open(&db_path()?)?;
    let mut rows = 0usize;
    for &i in &victims {
        let f = &files[i];
        fs::remove_file(&f.path).with_context(|| format!("removing {}", f.path.display()))?;
        rows += store.purge_tests(&f.tree, &f.system)?;
    }
    store.vacuum()?;

    println!(
        "Evicted {} eval file(s) ({} freed, {} test row(s) purged); {} of eval cache remains.",
        victims.len(),
        human_bytes(freed),
        rows,
        human_bytes(total - freed),
    );
    Ok(())
}

/// Prompt on stderr and read a yes/no answer from stdin; `true` only on an
/// explicit yes. A closed stdin (EOF, e.g. `--clean` in a pipe with no input)
/// reads as *no* — the safe default for a destructive action.
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write;
    // Prompt on stderr so a redirected stdout keeps only the machine-ish summary.
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        eprintln!(); // move past the prompt line on EOF
        return Ok(false);
    }
    Ok(is_yes(&line))
}

/// Whether a prompt answer means yes (`y`/`yes`, case- and space-insensitive).
fn is_yes(line: &str) -> bool {
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// The first 12 hex chars of a tree hash — enough to recognise, short enough to list.
fn short_tree(tree: &str) -> &str {
    &tree[..tree.len().min(12)]
}

/// Format a Unix time (seconds) as a UTC `YYYY-MM-DD` date, for the preview list.
fn fmt_date(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Inverse of [`days_from_civil`] (Howard Hinnant's `civil_from_days`): days
/// since 1970-01-01 back to `(year, month, day)`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// A byte count in binary units, e.g. `3.4 GiB` (exact `B` under 1 KiB).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1024"), Some(1024)); // bare = bytes
        assert_eq!(parse_size("500MB"), Some(500_000_000));
        assert_eq!(parse_size("4GiB"), Some(4 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1.5GB"), Some(1_500_000_000));
        assert_eq!(parse_size("2mo"), None); // a duration, not a size
        assert_eq!(parse_size("2026-07-15"), None); // a date
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration_secs("30d"), Some(30 * 86_400));
        assert_eq!(parse_duration_secs("2mo"), Some(2 * 2_592_000));
        assert_eq!(parse_duration_secs("1yr"), Some(31_536_000));
        assert_eq!(parse_duration_secs("1y"), Some(31_536_000));
        assert_eq!(parse_duration_secs("500MB"), None);
    }

    #[test]
    fn parses_dates() {
        // 2026-07-15 is 20649 days after the epoch.
        assert_eq!(parse_date("2026-07-15"), Some(20649 * 86_400));
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2026-7-15"), None); // not zero-padded
        assert_eq!(parse_date("2026-13-01"), None); // bad month
        assert_eq!(parse_date("notadate"), None);
    }

    #[test]
    fn spec_dispatch_is_disjoint() {
        assert_eq!(
            CleanSpec::parse("4GiB").unwrap(),
            CleanSpec::Budget(4 << 30)
        );
        assert!(matches!(
            CleanSpec::parse("2026-07-15").unwrap(),
            CleanSpec::Before(_)
        ));
        assert!(matches!(
            CleanSpec::parse("2mo").unwrap(),
            CleanSpec::Before(_)
        ));
        assert!(CleanSpec::parse("garbage").is_err());
    }

    /// Build an `Eval` with just the fields `victims` reads.
    fn ev(size: u64, mtime: u64) -> Eval {
        Eval {
            path: PathBuf::new(),
            tree: String::new(),
            system: String::new(),
            size,
            mtime,
        }
    }

    #[test]
    fn budget_evicts_least_recently_used_first() {
        // Three 100-byte files, distinct mtimes. Budget 250 keeps the two
        // newest (200 ≤ 250) and evicts the oldest.
        let files = [ev(100, 30), ev(100, 10), ev(100, 20)];
        let mut got = victims(&files, &CleanSpec::Budget(250));
        got.sort();
        assert_eq!(got, vec![1]); // index 1 has the oldest mtime (10)

        // Budget 50 fits nothing → evict all.
        let mut all = victims(&files, &CleanSpec::Budget(50));
        all.sort();
        assert_eq!(all, vec![0, 1, 2]);

        // Budget above the total evicts nothing.
        assert!(victims(&files, &CleanSpec::Budget(1000)).is_empty());
    }

    #[test]
    fn budget_evicts_tail_not_cheapest() {
        // Newest is big (doesn't fit), older two are small (would fit). Strict
        // LRU evicts the newest-that-overflows and everything OLDER — it must
        // not skip the big newest file to keep the older small ones.
        let files = [ev(300, 30), ev(50, 20), ev(50, 10)];
        let mut got = victims(&files, &CleanSpec::Budget(150));
        got.sort();
        assert_eq!(got, vec![0, 1, 2]);
    }

    #[test]
    fn before_evicts_older_than_cutoff() {
        let files = [ev(1, 100), ev(1, 200), ev(1, 300)];
        let mut got = victims(&files, &CleanSpec::Before(250));
        got.sort();
        assert_eq!(got, vec![0, 1]);
    }

    #[test]
    fn human_bytes_reads_well() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn confirmation_needs_an_explicit_yes() {
        for ok in ["y", "Y", "yes", "YES", " yes \n", "y\n"] {
            assert!(is_yes(ok), "{ok:?} should confirm");
        }
        for no in ["", "\n", "n", "no", "nope", "yep", "sure", "1"] {
            assert!(!is_yes(no), "{no:?} should NOT confirm");
        }
    }

    #[test]
    fn date_formatting_round_trips_the_cutoff_parser() {
        // fmt_date is the inverse of parse_date's civil arithmetic.
        assert_eq!(fmt_date(0), "1970-01-01");
        assert_eq!(fmt_date(20649 * 86_400), "2026-07-15");
        // Round-trip a spread of dates through days_from_civil -> civil_from_days.
        for &(y, m, d) in &[(1970, 1, 1), (2000, 2, 29), (2026, 7, 17), (2038, 12, 31)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
    }

    #[test]
    fn short_tree_is_a_prefix() {
        assert_eq!(
            short_tree("6ad2cd58bc5c3fe03106020942764b763300789b"),
            "6ad2cd58bc5c"
        );
        assert_eq!(short_tree("abc"), "abc"); // shorter than 12 is fine
    }
}
