-- The SQLite fact store's schema, embedded into `store.rs` via `include_str!`
-- (kept in its own file for SQL syntax highlighting). The `source`/`outcome`
-- integer enum-code mappings live in `store.rs` (Rust, not DDL) — the values
-- quoted in comments here are informative copies of that source of truth.
--
-- No migrations, ever (CLAUDE.md): change this schema freely and in place. The
-- whole store is a re-derivable cache, so the remedy for an incompatible
-- change is deleting `~/.cache/nix-npd`, never a compat shim.

-- The append-only observation log (DESIGN.md §3): the build driver appends a
-- `local`/`cache` fact here per drv. `drv_path` is stored stripped of its
-- constant `/nix/store/` prefix and `.drv` suffix (`evalfile::strip_drv`), and
-- `blocker`'s output paths of their `/nix/store/` prefix (`strip_out` — an
-- output path has no `.drv`); both restored on read. `source` and `outcome` are
-- small integer enum codes (`source_code`/`outcome_code` in `store.rs`), not
-- their English labels: `source` 0 = local build, 1 = cache (narinfo) probe;
-- `outcome` 0 = built, 1 = failed, 2 = dep-failed.
-- This is the one append-only, never-evicted table, so trimming its
-- per-row bytes is what compounds over time.
CREATE TABLE IF NOT EXISTS observation (
    id         INTEGER PRIMARY KEY,
    drv_path   TEXT    NOT NULL,
    source     INTEGER NOT NULL,
    outcome    INTEGER NOT NULL,
    when_      INTEGER NOT NULL,
    -- Newline-joined output paths whose validity re-decides this fact offline on
    -- a later run (DESIGN.md §5): for a `dep-failed`, the culprit dependency's
    -- outputs; for a `failed`, the drv's own outputs. NULL for a success.
    blocker    TEXT
) STRICT;
CREATE INDEX IF NOT EXISTS observation_drv ON observation (drv_path);

-- The `--tests` passthru.tests eval cache (DESIGN.md §4, §6). A test's drv is a
-- pure function of (tree, system, package-attr) — the source *tree*, not the
-- commit (see `model::Rev`) — so we cache per package and reuse across reviews at
-- a tree (a rebase/amend, or committing an as-is working tree, all hit).
--
-- The `(tree, system)` an eval belongs to is *interned* into `eval_key` and
-- referenced by its small integer `id`, rather than repeated as a 40-char tree
-- hash + system string on every row of the tables below. A handful
-- of distinct keys back thousands of test rows, so this is ~25% off the whole
-- `--tests` cache on real data (the biggest lever; DESIGN.md §4). It's also the
-- eviction unit: dropping an eval file (`--clean`) purges its key here, cascading
-- to the rows below.
CREATE TABLE IF NOT EXISTS eval_key (
    id     INTEGER PRIMARY KEY,
    tree   TEXT NOT NULL,
    system TEXT NOT NULL,
    UNIQUE (tree, system)
) STRICT;

-- `test_pkg` marks a package fully evaluated (present even when it has zero
-- tests, so a no-test package isn't re-evaluated every run); `test_drv` holds
-- each resolved `<pkg>.tests.<name>` drv (a package may contribute zero rows).
-- Drv paths are stored *stripped* of their constant `/nix/store/` prefix and
-- `.drv` suffix, exactly like the eval files (`evalfile::strip_drv`) — restored
-- on read.
-- `skipped` is the test's own meta-blocked bit — 0 = buildable, 1 = meta-skipped
-- (a Rust bool in `TestJob::skipped`; a test can be unsupported on this system
-- even when its package builds — an x86-only NixOS test on aarch64) — so it's
-- stored per test, not inferred from the package.
CREATE TABLE IF NOT EXISTS test_pkg (
    key_id   INTEGER NOT NULL REFERENCES eval_key (id),
    pkg_attr TEXT NOT NULL,
    PRIMARY KEY (key_id, pkg_attr)
) STRICT, WITHOUT ROWID;
-- The primary key includes `pkg_attr` (though `test_attr` alone would be unique
-- per key — it's the full `<pkg>.tests.<name>` path, embedding the package) so
-- the one read pattern, `WHERE key_id = ? AND pkg_attr IN (…)`, is a prefix scan
-- of the WITHOUT-ROWID clustering key itself — no secondary index, which would
-- otherwise store every column a second time.
CREATE TABLE IF NOT EXISTS test_drv (
    key_id    INTEGER NOT NULL REFERENCES eval_key (id),
    pkg_attr  TEXT NOT NULL,
    test_attr TEXT NOT NULL,
    drv_path  TEXT NOT NULL,
    skipped   INTEGER NOT NULL,
    PRIMARY KEY (key_id, pkg_attr, test_attr)
) STRICT, WITHOUT ROWID;

-- The patch-tree cache (DESIGN.md §8): maps a `--patch <A...B>` compare — its
-- anchor commit and sha-pinned expression — to the head *tree* npd reconstructed
-- by applying that compare's diff onto the anchor. It lets a *reproduction*
-- command's warm re-run skip the GitHub compare download: npd re-mints the
-- synthetic head over the cached tree (when its git objects survive) instead of
-- re-fetching. `anchor` and `expr` come straight from the command, so this needs
-- no knowledge of the original `--pr` run; the value is a tree hash, never the
-- diff. Re-derivable like everything else — a miss just re-downloads.
CREATE TABLE IF NOT EXISTS patch_tree (
    anchor TEXT NOT NULL,
    expr   TEXT NOT NULL,
    tree   TEXT NOT NULL,
    PRIMARY KEY (anchor, expr)
) STRICT, WITHOUT ROWID;
