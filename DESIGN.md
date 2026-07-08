# npd design

## 1. Purpose and scope

`npd` supports a **durable, iterative** nixpkgs workflow on a fixed set of
long-lived build machines with plenty of disk. It exists to make these cheap:

- evaluate a revision → the set of `attr → derivation` on each platform;
- diff two revisions (and, three-way, their merge base) to a set of changed attrs;
- learn whether a derivation is already substitutable from `cache.nixos.org`;
- build derivations locally, remembering the outcome (Nix keeps the log itself);
- render human-readable Markdown reports from all of the above;

…while **never repeating expensive work whose answer is already known**, and
while making it ergonomic to *deliberately* ignore the cache (build locally
instead of substituting; rebuild a success you suspect is flaky; skip a failure
you expect to just repeat).

### What `npd` is not

- Not a `nixpkgs-review` replacement. The build-result **classifier / comparison
  report** (the base×head delta grid, the composed status token) is a
  *presentation* concern that could live upstream in nixpkgs-review as a
  self-contained feature; `npd` keeps its own rendering for now.
- Not a re-implementation of Nix's primitives. Evaluation goes through
  `nix-eval-jobs`; building goes through `nix build` + the existing remote
  builders. `npd` owns the **memory** and the **orchestration**, not the plumbing.

## 2. The one load-bearing decision: key facts on `drvpath`

A **derivation path** (`/nix/store/<hash>-name.drv`) is the identity of a build
*recipe* — a hash of its inputs. An **output path** (`/nix/store/<hash>-name`) is
the identity of a produced *artifact*. They differ, and the difference dictates
the schema:

- A **failed** build has no output but always has a drvpath. Keying on drvpath
  lets us remember failures; keying on output path can only remember successes
  (that's all a binary cache stores).
- The same drvpath recurring in two different commits/PRs is automatically **one**
  cache entry — cross-review sharing falls out for free.
- Output paths are many-to-one with drvs for fixed-output/CA derivations (one
  source path, countless fetch drvs), so they're a poor primary key anyway.

Therefore: **build facts are keyed on `drvpath`.** Output paths are used only
where they are the right key (narinfo / substituter presence).

## 3. Two kinds of facts

There are only two, and collapsing everything else into the second is a
deliberate simplification (it dropped out of the design discussion):

| fact | key | discipline | storage |
| --- | --- | --- | --- |
| **eval** — attr→drv map | `(commit, system, config)` | **pure** → cache forever, never invalidate | one flat file per key |
| **observation** — one build/probe event | `drvpath` (or output path for `Cache`) | **append-only log** — never overwrite | SQLite |

An eval at a fixed `(commit, system, config)` is deterministic, so its result is
valid forever. Everything else is an **observation**: a single event, from some
`Source` — a `Local` build we ran, or `Cache` (narinfo) presence on a
substituter — stamped with `when`. We append and never discard, which is what
makes flakiness representable (multiple observations of one drv with differing
outcomes).

**A cache probe is an observation too** — "is output H in the cache right now"
is just something we observed at time `when`, recorded so a later run needn't
re-probe. There is no eviction and no TTL, which keeps full history (a drv that
went green → red → green is visible) under one log.

> **History:** `npd` once also consulted Hydra (a `HydraJob` source + an `npd
> hydra` command). That was dropped: the public Hydra API has no reverse
> drvpath→build lookup, so its forward-job answers *drift* (a different drv than
> ours) and are unreliable to key facts on. `npd` now consults only
> `cache.nixos.org` (drv-precise) and local builds (ground truth).

## 4. Storage

Everything `npd` stores is re-derivable, so it lives under
`dirs::cache_dir()/nix-npd` (i.e. `~/.cache/nix-npd`), like `npc`. The records are
all cache: losing them costs re-evaluation / re-building, not correctness. `npd`
keeps **no gcroots** — a built output may be GC'd, but the *observation* that it
built survives, and that's the fact we actually need; if the output is wanted
again, Nix rebuilds or substitutes it.

The two fact kinds have opposite access patterns, so they get different backends.

**Evals → one flat file per `(commit, system, profile)`** under `evals/`, sorted
`attr\tdrv` lines (empty drv = no derivation; `src/eval.rs`). The drv is stored
stripped of its constant `/nix/store/…​.drv` prefix/suffix, and the whole file is
zstd-compressed (default level) — together ~3× smaller (~11 MB → ~3.4 MB). An eval is bulk,
write-once, read-as-a-whole data whose *only* use is to be diffed against another
eval, so a file beats SQLite on every axis that matters here:

- **smaller** — ~3.4 MB compressed (vs ~11 MB raw, ~22 MB in SQLite: no per-row
  overhead, no `(run_id, attr)` index duplicating the data);
- **faster to diff** — both files are sorted by attr, so the changed set is a
  linear two-pointer merge over borrowed slices (~16 ms) rather than ~114k
  primary-key point-lookups (~94 ms). The cross-cutting SQL queries that would
  have justified a table never materialised (we only ever diff);
- **evictable** — when the cache grows too big, delete whole eval files for old
  commits; no `VACUUM` of a monolith. (The "millions of tiny files" failure mode
  is about a file *per attr*; one file per *eval* is ~two files per review.)

Writes are atomic (temp + `rename`) so a crash can't leave a truncated file that
would poison the cache. Compression (zstd, ~7×) is left off for now: it would
trade the fast mmap-and-merge for decompression time, and disk is cheap.

**Observations → SQLite** (`npd.sqlite`), where the append-only log actually
wants an engine: indexed lookup by `drvpath`, transactional appends, no torn
writes. It stays tiny (KBs) — this is what SQLite is *for* here, and nothing
else lives in it. Build logs are stored nowhere: Nix keeps them under
`/nix/var/log/nix/drvs` (`nix log <drv>`, success or failure).

```
~/.cache/nix-npd/
  npd.sqlite                    # observation log (tiny)
  evals/<commit>-<sys>-<profile>-v<n>.tsv.zst  # attr→drv maps (zstd), one file per eval
  logs/eval-<commit>-<sys>.log  # nix-eval-jobs stderr (tracebacks), per eval
```

`<drv-hash>` is the 32-char hash component of the drvpath.

## 5. The observation log and the build-policy predicate

Every local build appends an `Observation` (source, outcome, when, duration,
machine, log). The ergonomics the workflow needs are then a **pure predicate**
over that log plus substituter presence:

- never observed, or forced → **build**
- a `LOCAL` success exists, `--recheck` off → **skip (ok)**
- substitutable success, `--prefer-local`/`--recheck` off → **skip (ok)**
- only failures observed, `--retry` off → **skip (fail)**
- otherwise → **build**

So the cache-bypass knobs are just fields on the policy: `recheck` (rebuild a
suspected-flaky success), `retry` (re-attempt a known failure), `prefer_local`
(don't trust a substituted success — build it here). See `BuildPolicy::decide`
in `src/model.rs`.

**Staying instant when cached.** The driver loads every target's history in one
SQLite query, and only *probes the cache* for drvs it doesn't already know are
built (locally, or from a `Cache` observation a prior run recorded); those probes
run concurrently (`cache::in_cache_many`). So a changed set whose facts are all
known costs one query and no network — the whole build set is decided in
milliseconds. The actual build is a single batched `nix build` piped through
`nom` for the live tree, from which we recover, per drv, its outcome (built /
direct failure / dependency cascade) and duration.

## 6. Evaluation, its cache key, and the three-way diff

**The cache key is `(commit, system, config)`, and it is not a can of worms —
provided `npd` owns the config.** What determines the attr→drv map is the
nixpkgs revision, the platform, and the nixpkgs *config* (allowlists like
`allowBroken`/`allowUnfree`/`allowUnsupportedSystem`, `permittedInsecurePackages`,
overlays, `config.allowAliases`, …). The trap is letting a user pass arbitrary
Nix as config — that isn't cleanly hashable. `npd` avoids it by **defining the
eval config itself**: a single canonical profile (or a small set of named
profiles), so `config` is a short enumerable label, not arbitrary code. The key
is then just `(commit, system, profile)`, plus an `npd`-eval-version tag bumped
if we ever change *how* we invoke `nix-eval-jobs`.

Caching is sound because nixpkgs evaluation is deterministic given those inputs
(drv paths are content-addressed by their inputs, stable across time and
machines); IFD is still deterministic, and impurities like `currentSystem` are
fixed by the `system` key. So "should we cache evals?" — yes, unreservedly, once
`npd` owns the config.

`eval(commit, system)` → `{attr: AttrEval}` via `nix-eval-jobs` (cached, pure).
A two-way diff is a set-diff on `(attr, drv_path)`. The **three-way** diff also
evaluates the **merge base** of the two commits, which classifies each changed
attr the way a git three-way merge does:

- changed by *this side* only (base == merge-base, differs at head),
- changed by the *other side* only (head == merge-base, differs at base — e.g.
  the target branch advanced / a mass rebuild landed),
- changed by *both* (all three differ — genuine interaction).

This is the main capability nixpkgs-review lacks; it is nearly free once
`eval` is a cached primitive.

## 7. Cache facts — the one remote signal

The only remote fact `npd` gathers is **narinfo presence** on `cache.nixos.org`:
`HEAD /<out-hash>.narinfo` → does an already-built output for *this exact drv*
exist to substitute? It is drv-precise and drift-free, but **success-only** (a
404 conflates never-built / failed / GC-evicted — it can never assert a
failure). A hit is recorded as a `Cache`/`Built` observation so a later run
skips the probe; a miss records nothing (re-probing is cheap, and cache state
can change under us). Ground truth for anything a narinfo can't answer is a
**local build** (§5).

> Why not Hydra? The public hydra.nixos.org API has **no reverse index** from a
> store path to a build (search is name-keyed, 500s on paths; no `/store-path`
> endpoint). Its forward job endpoint (`/job/.../latest`) returns the *latest*
> build's drv, which routinely **drifts** from ours — so it can't be keyed on
> without inventing false regressions. `npd` dropped it.
>
> Upstream opportunity (separate): Hydra already indexes `BuildOutputs.path` and
> `Builds.drvpath`; a small PR adding an exact `drvpath`/`path` lookup would give
> a real reverse endpoint (surfacing failures + cached flags), which `npd` could
> then consult in place of a local build for drvs Hydra actually built.

## 8. Reports

Markdown, grouped by the **delta** each attr underwent. Each side reduces to one
of five states — `✅` built, `❌` failed (direct), `🚫` blocked (a dependency
failed — the transitive/cascade case, kept distinct from a direct failure), `➖`
absent (no such attr on that side — a *known* fact, never a `?`), `❓` unbuilt
(has a drv, no fact yet; only under `--no-build`). A section is one `(base, head)`
state pair, and its header **is** a composable `before → after` token (one emoji
per side) — no per-row glyphs; the section a row lands in carries all the meaning.
Sections are ordered worst-delta-first and folded in `<details>` (open when the
state changed, collapsed when `before == after`). Attrs that share a derivation
are collapsed onto one line (`a = b = c`, shortest attr first), like
`nixpkgs-review`'s aliases — npd gets this for free from its drvpath keying.

`npd report` is not merely read-only: with defaults (`head` = `HEAD`, `base` =
merge-base with `master`) it first **builds both sides of the changed set**
(skipping anything already known or substitutable), so a fresh report has a real
state for every row rather than a wall of `❓`. `--no-build` opts back into pure
read-only rendering.

## 9. Build order (spine first; resist features until the spine carries weight)

The spine is implemented (✓).

1. ✓ cached `eval(commit, system)` → attr→drv map (`nix-eval-jobs`), evals run
   in parallel under a RAM-slot budget.
2. ✓ two-way diff, then the three-way (merge-base) diff.
3. ✓ the drvpath-keyed observation store + `BuildPolicy` + a local build driver
   that consults/appends it: one batched `nom` build, parallel cache probing,
   `DepFailed`/cascade detection, and per-drv duration.
4. ✓ `Cache` facts (narinfo), recorded as observations.
5. ✓ Markdown report classifying the changed set, building both sides first so
   there are no `?`.

All of the above is driven by a single `npd [base] [head]` command (the
eval/diff/build/report primitives are internal modules, not subcommands).

Open refinements: remote-builder fan-out; a `Local`-vs-`Cache` fidelity probe
(from-source build vs. substitution).

**Known gotcha — `nix-eval-jobs` on macOS is pathologically slow.** Measured
~1.5 attrs/s on an `aarch64-darwin` VM vs ~155 attrs/s on `aarch64-linux` on the
same hardware — ~100× — even though a plain `nix eval` of a single package is
instant on both. So it's specific to `nix-eval-jobs`' worker-based full-set
streaming on darwin, not core Nix eval, and it's not fixable by npd's
worker/memory knobs (disabling the memory cap didn't help). Practical upshot:
run `npd` on the Linux build boxes (its intended home); a full-set eval on a Mac
is effectively a non-starter until this is fixed upstream. The eval progress bar
shows a live timer so a slow eval reads as working, not hung.

## 10. Open questions

- The report classifier's eventual home (§8) — revisit when we get to reports.

Resolved earlier and recorded for context:

- *Eval cache key* → `(commit, system, profile)` with an eval-version tag; not a
  can of worms because `npd` owns the config (§6).
- *Concurrency* → not handled. One machine is the driver and keeps its store
  local; multiple drivers keep independent stores, exactly as the Nix store
  already works. The append-only design stays friendly to revisiting this.
- *Cache facts lifetime* → append-only observations, no eviction/TTL. A `Cache`
  observation records the drvpath, so staleness can't affect correctness (§3).
- *Remote facts* → narinfo on `cache.nixos.org` only; Hydra was dropped (§7).
- *Storage* → SQLite (`npd.sqlite`) under `dirs::cache_dir()/nix-npd`; all re-derivable cache (§4).
