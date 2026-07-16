//! The eval cache files and their diff (DESIGN.md §4, §6).
//!
//! Each eval is a standalone file under `<cache>/<system>/`, not SQLite rows. It's a
//! bulk, write-once, read-as-a-whole artifact — the only thing we ever do with it
//! is diff two of them — so a flat file is both smaller (no per-row / index
//! overhead; ~11 MB vs ~22 MB in SQLite) and lets us evict by whole file (drop
//! old commits' evals) without vacuuming a monolithic DB. The format is one
//! `attr\tdrv` line per attr, sorted by attr (empty drv = no derivation), plus a
//! third field `!` on the few rows npd skips (meta-blocked:
//! broken/unsupported/insecure), so the diff is a linear two-pointer merge.
//!
//! The drv column is stored *stripped*: `/nix/store/<h>-<n>.drv` is written as
//! just `<h>-<n>` (see `strip_drv`), since that prefix/suffix is constant across
//! every line — ~15 B/line, ~15% off the file. Reconstruction (`restore_drv`) is
//! one concat per changed row, so it costs nothing on the unchanged majority the
//! merge skips. The format is strict — every drv is a `/nix/store` `.drv` or
//! absent, matching the rest of npd (e.g. `cache::store_hash`) — with no fallback
//! for other shapes: on a format change, delete `~/.cache/nix-npd` so old files
//! are regenerated rather than mis-parsed as if they were stripped.
//!
//! The whole (stripped) TSV is then zstd-compressed on disk (~3x smaller at the
//! default level; higher levels and a two-file split bought little). The diff
//! consumes each file as a single stream: decompressed on its own thread, merged
//! line-by-line, so no whole-file buffer is ever materialized (see
//! [`changed_set`]).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::thread;

use anyhow::{Context, Result};

use crate::model::AttrEval;
use crate::paths::cache_root;

/// The cache file for one `(tree, system)` eval. Keyed on the git *tree*, not
/// the commit: the eval depends only on the source content, so two commits with
/// the same tree share one file (see [`crate::model::Rev`], DESIGN.md §6). The
/// eval format carries no version tag: everything under `~/.cache/nix-npd` is
/// re-derivable, so a change to the file format, the eval config (`EVAL_CONFIG`
/// in `eval.rs`), or *how* `nix-eval-jobs` is invoked is invalidated by deleting
/// the cache, not by coexisting versions (no migration code — see CLAUDE.md).
pub fn eval_path(tree: &str, system: &str) -> Result<PathBuf> {
    Ok(cache_root()?.join(system).join(format!("{tree}.tsv.zst")))
}

/// Write an eval to its file, sorted by attr, zstd-compressed, atomically: a
/// uniquely-named temp file in the *same directory* (rename is only atomic
/// within one filesystem, so the system temp dir won't do), then rename into
/// place. A crash can never leave a truncated file that would poison the cache,
/// and concurrent writers of the same key can't tread on each other's temp.
pub fn write_eval(path: &Path, attrs: &[AttrEval]) -> Result<()> {
    let mut rows: Vec<(&str, &str, bool)> = attrs
        .iter()
        .map(|a| {
            (
                a.attr.as_str(),
                a.drv_path.as_deref().map(strip_drv).unwrap_or(""),
                a.skipped,
            )
        })
        .collect();
    rows.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let mut buf = String::with_capacity(rows.len() * 96);
    for (attr, drv, skipped) in rows {
        buf.push_str(attr);
        buf.push('\t');
        buf.push_str(drv);
        // A third field only on the (few) skipped (meta-blocked) rows: `!`.
        if skipped {
            buf.push_str("\t!");
        }
        buf.push('\n');
    }
    // Level 0 = zstd's default level (currently 3); pass the sentinel rather than
    // a number so we track the library's default rather than pinning it.
    let compressed = zstd::encode_all(buf.as_bytes(), 0).context("compressing eval")?;
    let dir = path.parent().expect("eval path has a parent");
    fs::create_dir_all(dir).context("creating eval dir")?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir).context("creating temp eval file")?;
    tmp.write_all(&compressed)
        .context("writing temp eval file")?;
    tmp.persist(path).context("renaming eval into place")?;
    Ok(())
}

/// Read and decompress an eval file into its TSV text — [`write_eval`]'s
/// mirror for the format-round-trip tests; the production diff streams the
/// file instead (see [`changed_set`]).
#[cfg(test)]
fn read_eval(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let tsv = zstd::decode_all(&bytes[..])
        .with_context(|| format!("decompressing {}", path.display()))?;
    String::from_utf8(tsv).with_context(|| format!("{} is not valid UTF-8", path.display()))
}

/// The on-disk form of a drv path: strip the constant `/nix/store/` prefix and
/// `.drv` suffix; [`restore_drv`] re-adds them. Every drv `nix-eval-jobs` emits
/// has this exact shape (an errored attr carries no drv and is stored as an empty
/// field, so this is only ever called on a real path). Shared with the `--tests`
/// SQLite cache (`store.rs`), which stores its drvs stripped for the same reason.
pub(crate) fn strip_drv(drv: &str) -> &str {
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
pub(crate) fn restore_drv(field: Option<&str>) -> Option<String> {
    field.map(|s| format!("/nix/store/{s}.drv"))
}

/// Mark an eval file as used *right now*, so LRU eviction (`--clean`,
/// DESIGN.md §4) treats a cache *hit* as recent. Reading a file doesn't touch
/// its mtime, so a base eval reused across dozens of reviews would otherwise
/// look as old as its first write and be evicted before a one-off; stamping it
/// on every review that consults it makes mtime a true last-*used* time. A
/// freshly written eval is already current, so this is only for the hits.
/// Best-effort: a failure to re-stamp only risks the file looking staler than
/// it is (evicted a little early, then re-derived), never a correctness bug.
pub fn touch_eval(path: &Path) {
    if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
}

/// One parsed eval row, borrowing from its line: attr, stored-form drv, and the
/// meta-blocked bit.
type EvalRow<'a> = (&'a str, Option<&'a str>, bool);

/// Parse one eval-file line into an [`EvalRow`] (no allocation). The drv is
/// left in its stored form (see [`strip_drv`]); since that encoding is
/// injective, the merge can compare stored fields directly and only
/// [`restore_drv`] the few rows it emits.
fn parse_line(l: &str) -> EvalRow<'_> {
    let mut fields = l.splitn(3, '\t');
    let attr = fields.next().unwrap_or(l);
    let drv = fields.next().unwrap_or("");
    let skipped = fields.next() == Some("!");
    (attr, if drv.is_empty() { None } else { Some(drv) }, skipped)
}

/// Parse a whole eval file's text into [`EvalRow`]s, borrowing from `buf`.
/// The production diff never materializes this (it streams lines — see
/// [`changed_set`]); this is [`read_eval`]'s counterpart for the tests that
/// check the on-disk format round-trips.
#[cfg(test)]
fn parse_eval(buf: &str) -> Vec<EvalRow<'_>> {
    buf.lines().map(parse_line).collect()
}

/// One changed attr between two evals: its drv and meta-blocked bit on each side
/// (`None` = absent/no derivation there).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedAttr {
    pub attr: String,
    pub base_drv: Option<String>,
    pub head_drv: Option<String>,
    pub base_skipped: bool,
    pub head_skipped: bool,
}

// The diff pipeline: each file is decompressed on its own thread (the two
// sides overlap) and handed to the consuming merge in bounded chunks, which
// walks both line streams in lockstep. Compared to materializing each ~11 MB
// TSV and a ~114k-row Vec per side, streaming both halves the warm-run wall
// time (decompression pipelines with parsing+merging instead of preceding it,
// and none of the big buffers are ever allocated or page-faulted in) and cuts
// peak memory ~3× — measured over two ~113k-attr evals.

/// Decompressed bytes of one eval file, produced on a decoder thread and
/// consumed as a `Read`. A producer-side error (open/read/decompress) arrives
/// in-band as the next `read` result; producer EOF is a closed channel.
struct ChunkReader {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    cur: Vec<u8>,
    pos: usize,
}

impl std::io::Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos == self.cur.len() {
            match self.rx.recv() {
                Ok(Ok(c)) => {
                    self.cur = c;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0), // producer finished: EOF
            }
        }
        let n = (self.cur.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Chunk granularity of a decoder thread, and its consumer's buffer size.
const DECODE_CHUNK: usize = 256 * 1024;

/// Spawn a thread in `scope` that streams `path` through a zstd decoder,
/// sending [`DECODE_CHUNK`]-sized chunks down a bounded channel (so a fast
/// producer stays ~1 MB ahead, never the whole file). Returns the consuming
/// end. The thread exits when the file is drained, on error, or when the
/// consumer hangs up mid-file (e.g. the merge failed on the other side).
fn spawn_eval_decoder<'scope>(
    path: PathBuf,
    scope: &'scope thread::Scope<'scope, '_>,
) -> BufReader<ChunkReader> {
    use std::io::Read;
    let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(4);
    scope.spawn(move || {
        let mut dec = match fs::File::open(&path).and_then(zstd::stream::read::Decoder::new) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        loop {
            // Fill each chunk completely (a zstd decoder returns short reads)
            // so the channel traffics in as few sends as possible.
            let mut chunk = vec![0u8; DECODE_CHUNK];
            let mut filled = 0;
            while filled < chunk.len() {
                match dec.read(&mut chunk[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                }
            }
            chunk.truncate(filled);
            if chunk.is_empty() || tx.send(Ok(chunk)).is_err() {
                return;
            }
        }
    });
    BufReader::with_capacity(
        DECODE_CHUNK,
        ChunkReader {
            rx,
            cur: Vec::new(),
            pos: 0,
        },
    )
}

/// One side of the changed-set merge: the current attr-sorted [`EvalRow`]
/// (borrowing from the cursor's own buffer) plus fallible advancement. The
/// merge is written once over this; implementations are the streaming line
/// reader ([`LineCursor`]) and in-memory parsed rows ([`SliceCursor`]).
trait RowCursor {
    /// The current row, or `None` once exhausted.
    fn row(&self) -> Option<EvalRow<'_>>;
    fn advance(&mut self) -> Result<()>;
}

/// [`RowCursor`] over a streamed eval file, one reused line buffer per side.
struct LineCursor<R: BufRead> {
    src: R,
    /// Names the file in read errors (which surface here, not at open — the
    /// decoder thread reports in-band through the stream).
    path: PathBuf,
    cur: String,
    done: bool,
}

impl<R: BufRead> LineCursor<R> {
    fn new(src: R, path: PathBuf) -> Result<Self> {
        let mut c = LineCursor {
            src,
            path,
            cur: String::new(),
            done: false,
        };
        c.advance()?;
        Ok(c)
    }
}

impl<R: BufRead> RowCursor for LineCursor<R> {
    fn row(&self) -> Option<EvalRow<'_>> {
        (!self.done).then(|| parse_line(&self.cur))
    }
    fn advance(&mut self) -> Result<()> {
        self.cur.clear();
        let n = self
            .src
            .read_line(&mut self.cur)
            .with_context(|| format!("streaming {}", self.path.display()))?;
        if n == 0 {
            self.done = true;
        } else if self.cur.ends_with('\n') {
            self.cur.pop();
        }
        Ok(())
    }
}

/// [`RowCursor`] over already-parsed rows ([`changed_tests`] and the unit
/// tests); infallible.
struct SliceCursor<'a> {
    rows: &'a [EvalRow<'a>],
    i: usize,
}

impl RowCursor for SliceCursor<'_> {
    fn row(&self) -> Option<EvalRow<'_>> {
        self.rows.get(self.i).copied()
    }
    fn advance(&mut self) -> Result<()> {
        self.i += 1;
        Ok(())
    }
}

/// A [`ChangedAttr`] for a row present only on the base side (skipped if it
/// has no drv — an eval error is treated as absent).
fn base_only(r: &EvalRow) -> Option<ChangedAttr> {
    r.1.is_some().then(|| ChangedAttr {
        attr: r.0.to_string(),
        base_drv: restore_drv(r.1),
        head_drv: None,
        base_skipped: r.2,
        head_skipped: false,
    })
}

/// [`base_only`]'s mirror for a row present only on the head side.
fn head_only(r: &EvalRow) -> Option<ChangedAttr> {
    r.1.is_some().then(|| ChangedAttr {
        attr: r.0.to_string(),
        base_drv: None,
        head_drv: restore_drv(r.1),
        base_skipped: false,
        head_skipped: r.2,
    })
}

/// The changed rows between two attr-sorted sides: one [`ChangedAttr`] for
/// each attr whose drv *or* meta-blocked bit differs (meta isn't part of the
/// drv hash, so (un)marking a package skipped can change nothing but the bit —
/// still a review event worth a row), via a linear two-pointer merge. Only the
/// (few) changed rows are allocated.
fn merge_rows(mut b: impl RowCursor, mut h: impl RowCursor) -> Result<Vec<ChangedAttr>> {
    let mut out = Vec::new();
    loop {
        // Decide what to emit and which side(s) to advance while the rows
        // borrow the cursors, then act once the borrows are released.
        let (emit, adv_b, adv_h) = match (b.row(), h.row()) {
            (None, None) => break,
            (Some(br), None) => (base_only(&br), true, false),
            (None, Some(hr)) => (head_only(&hr), false, true),
            (Some(br), Some(hr)) => match br.0.cmp(hr.0) {
                std::cmp::Ordering::Less => (base_only(&br), true, false),
                std::cmp::Ordering::Greater => (head_only(&hr), false, true),
                std::cmp::Ordering::Equal => {
                    let changed = br.1 != hr.1 || br.2 != hr.2;
                    let emit = changed.then(|| ChangedAttr {
                        attr: br.0.to_string(),
                        base_drv: restore_drv(br.1),
                        head_drv: restore_drv(hr.1),
                        base_skipped: br.2,
                        head_skipped: hr.2,
                    });
                    (emit, true, true)
                }
            },
        };
        out.extend(emit);
        if adv_b {
            b.advance()?;
        }
        if adv_h {
            h.advance()?;
        }
    }
    Ok(out)
}

/// The changed set between two cached evals (keyed by their git trees),
/// streaming both eval files through [`merge_rows`]: each side is decompressed
/// on its own thread ([`spawn_eval_decoder`]) and consumed line-by-line
/// ([`LineCursor`]), so the two decompressions overlap each other *and* the merge.
pub fn changed_set(base_tree: &str, head_tree: &str, system: &str) -> Result<Vec<ChangedAttr>> {
    changed_set_files(
        &eval_path(base_tree, system)?,
        &eval_path(head_tree, system)?,
    )
}

/// [`changed_set`] on explicit file paths (separable for tests).
fn changed_set_files(bpath: &Path, hpath: &Path) -> Result<Vec<ChangedAttr>> {
    thread::scope(|s| {
        // Both decoders are spawned before either cursor blocks on its first
        // chunk, so the two sides start decompressing together.
        let brd = spawn_eval_decoder(bpath.to_path_buf(), s);
        let hrd = spawn_eval_decoder(hpath.to_path_buf(), s);
        let b = LineCursor::new(brd, bpath.to_path_buf())?;
        let h = LineCursor::new(hrd, hpath.to_path_buf())?;
        merge_rows(b, h)
    })
}

/// [`merge_rows`] over two in-memory row slices — the merge's pure spelling,
/// used by [`changed_tests`] and the unit tests.
fn diff(b: &[EvalRow], h: &[EvalRow]) -> Vec<ChangedAttr> {
    merge_rows(SliceCursor { rows: b, i: 0 }, SliceCursor { rows: h, i: 0 })
        .expect("slice cursors are infallible")
}

/// Diff two `test_attr → (drv, skipped)` maps (the `--tests` cache's shape, full
/// drv paths) with exactly [`diff`]'s semantics, so test rows classify
/// (regression / fixed / new / meta-only …) like any full-set attr.
pub fn changed_tests(
    base: &std::collections::HashMap<String, (String, bool)>,
    head: &std::collections::HashMap<String, (String, bool)>,
) -> Vec<ChangedAttr> {
    fn rows(m: &std::collections::HashMap<String, (String, bool)>) -> Vec<EvalRow<'_>> {
        let mut v: Vec<EvalRow<'_>> = m
            .iter()
            .map(|(attr, (drv, skipped))| (attr.as_str(), Some(strip_drv(drv)), *skipped))
            .collect();
        v.sort_unstable_by_key(|r| r.0);
        v
    }
    diff(&rows(base), &rows(head))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_eval_advances_mtime() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("npd-touch-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("e.tsv.zst");
        fs::write(&path, b"contents").unwrap();
        // Backdate the file, then touch it: mtime must move forward and the
        // contents must survive (a write-open must not truncate).
        let old = SystemTime::now() - Duration::from_secs(3600);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        touch_eval(&path);
        let after = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(after > old, "touch_eval should advance the mtime");
        assert_eq!(fs::read(&path).unwrap(), b"contents");
        let _ = fs::remove_dir_all(&dir);
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
        let ae = |attr: &str, drv: Option<&str>, skipped: bool| AttrEval {
            attr: attr.into(),
            drv_path: drv.map(str::to_string),
            skipped,
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
        // only the skipped row carries the third `!` field (sorted by attr:
        // bad, br, hello). The file is zstd-compressed, so read it back through
        // the same helper the diff uses.
        let raw = read_eval(&path).unwrap();
        assert_eq!(raw, "bad\t\nbr\tb-br\t!\nhello\ta-hello\n");

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

    /// A [`ChangedAttr`] from stored-form drvs, for expected values.
    fn ca(
        attr: &str,
        base: Option<&str>,
        head: Option<&str>,
        base_skipped: bool,
        head_skipped: bool,
    ) -> ChangedAttr {
        ChangedAttr {
            attr: attr.into(),
            base_drv: restore_drv(base),
            head_drv: restore_drv(head),
            base_skipped,
            head_skipped,
        }
    }

    #[test]
    fn diff_emits_only_changed_rows() {
        // Both lists sorted by attr, as parse_eval guarantees.
        let b = [
            ("dropped", Some("d1"), false),
            ("errored.base", None, false), // eval error on base only: no row
            ("flip", Some("f1"), true),    // meta-only unmarking: row, same drv
            ("gone.err", Some("g1"), false), // drv on base, eval error at head
            ("rebuilt", Some("r1"), false),
            ("same", Some("s1"), false),
        ];
        let h = [
            ("added", Some("a1"), false),
            ("errored.head", None, false), // eval error on head only: no row
            ("flip", Some("f1"), false),
            ("gone.err", None, false),
            ("rebuilt", Some("r2"), false),
            ("same", Some("s1"), false),
        ];
        let got = diff(&b, &h);
        let want = vec![
            ca("added", None, Some("a1"), false, false),
            ca("dropped", Some("d1"), None, false, false),
            ca("flip", Some("f1"), Some("f1"), true, false),
            ca("gone.err", Some("g1"), None, false, false),
            ca("rebuilt", Some("r1"), Some("r2"), false, false),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn diff_drains_tails() {
        // One list ends first; the other's remainder must still be emitted
        // (with its no-drv rows skipped).
        let b = [("a", Some("a1"), false)];
        let h = [
            ("a", Some("a1"), false),
            ("y", None, false),
            ("z", Some("z1"), true),
        ];
        assert_eq!(diff(&b, &h), vec![ca("z", None, Some("z1"), false, true)]);
        assert_eq!(diff(&h, &b), vec![ca("z", Some("z1"), None, true, false)]);
        assert_eq!(diff(&[], &[]), vec![]);
    }

    #[test]
    fn changed_set_streams_real_files() {
        // End-to-end over the real on-disk shape: write two evals with
        // write_eval, diff them through the streaming path (decoder threads +
        // line cursors), and expect exactly diff's semantics.
        let ae = |attr: &str, drv: Option<&str>, skipped: bool| AttrEval {
            attr: attr.into(),
            drv_path: drv.map(str::to_string),
            skipped,
        };
        let dir = std::env::temp_dir().join(format!("npd-stream-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let bpath = dir.join("b.tsv.zst");
        let hpath = dir.join("h.tsv.zst");
        write_eval(
            &bpath,
            &[
                ae("dropped", Some("/nix/store/d1.drv"), false),
                ae("errored", None, false),
                ae("rebuilt", Some("/nix/store/r1.drv"), false),
                ae("same", Some("/nix/store/s1.drv"), false),
            ],
        )
        .unwrap();
        write_eval(
            &hpath,
            &[
                ae("added", Some("/nix/store/a1.drv"), true),
                ae("errored", None, false),
                ae("rebuilt", Some("/nix/store/r2.drv"), false),
                ae("same", Some("/nix/store/s1.drv"), false),
            ],
        )
        .unwrap();
        let got = changed_set_files(&bpath, &hpath).unwrap();
        let want = vec![
            ca("added", None, Some("a1"), false, true),
            ca("dropped", Some("d1"), None, false, false),
            ca("rebuilt", Some("r1"), Some("r2"), false, false),
        ];
        assert_eq!(got, want);
        // A missing file must error (through the in-band producer error).
        assert!(changed_set_files(&dir.join("nope.tsv.zst"), &hpath).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn changed_tests_matches_diff_semantics() {
        let m = |kv: &[(&str, &str, bool)]| {
            kv.iter()
                .map(|(a, d, b)| (a.to_string(), (format!("/nix/store/{d}.drv"), *b)))
                .collect::<std::collections::HashMap<_, _>>()
        };
        let base = m(&[
            ("pkg.tests.dropped", "d1", false),
            ("pkg.tests.flip", "f1", true),
            ("pkg.tests.same", "s1", false),
        ]);
        let head = m(&[
            ("pkg.tests.added", "a1", false),
            ("pkg.tests.flip", "f1", false),
            ("pkg.tests.same", "s1", false),
        ]);
        // Sorted by attr, full drv paths restored, meta-only flip kept.
        let got = changed_tests(&base, &head);
        let want = vec![
            ca("pkg.tests.added", None, Some("a1"), false, false),
            ca("pkg.tests.dropped", Some("d1"), None, false, false),
            ca("pkg.tests.flip", Some("f1"), Some("f1"), true, false),
        ];
        assert_eq!(got, want);
    }
}
