# npd

Read DESIGN.md before making non-trivial changes; it is the source of truth for
the architecture and is kept up to date as part of any change that affects it.

## Commit directly to `main`

Development happens on `main`. Don't create feature branches or open pull
requests for changes here — commit straight to `main`.

## No backward compatibility, ever

npd has exactly one user, no releases, and no deployments. Everything it stores
(`~/.cache/nix-npd`) is a re-derivable cache. Therefore (DESIGN.md §1):

- Never write migration code: no SQLite schema upgrades or `ALTER TABLE`/purge
  steps for data an older version wrote, no readers or fallbacks for previous
  file formats, no comments like "may linger on old databases".
- Change formats in place. If existing cached data would become wrong to read,
  invalidate instead of migrating: bump `EVAL_VERSION` (src/eval.rs) for eval
  files, or just note that `~/.cache/nix-npd` should be deleted.
- When removing a feature, remove all of it in the same change — enum variants,
  struct fields, table columns, parsing, tests, and doc references. Don't keep
  dead fields around because they might be useful later.
