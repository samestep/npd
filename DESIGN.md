# npd design

## 1. Purpose and scope

`npd` supports a **durable, iterative** nixpkgs workflow on a fixed set of
long-lived build machines with plenty of disk. It exists to make these cheap:

- evaluate a revision → the set of `attr → derivation` on each platform;
- diff two revisions to a set of changed attrs;
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

### No backward compatibility, ever

`npd` has exactly one user, no releases, and no deployments, and everything it
stores is a re-derivable cache (§4). So there is no such thing as "legacy data"
or an "old format" to support:

- **Never write migration code** — no schema upgrades, no purges of rows an
  older version wrote, no readers for previous file formats, no "this column
  may linger" tolerance. Change the current format in place.
- If a format change would make existing cached data wrong to read, the remedy
  is invalidation, not compatibility: bump `EVAL_VERSION` (eval files under a
  different version are simply never read) or delete `~/.cache/nix-npd`.
- When a feature is removed, remove **all** of it in the same change: enum
  variants, struct fields, table columns, parsing, and doc references. Dead
  "maybe useful later" fields are cruft; re-add them when they're actually used.

(Design *rationale* for dropped approaches — e.g. why Hydra isn't consulted,
§7 — is worth keeping in this document. Code paths for them are not.)

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

**Evals → one flat file per `(commit, system)`** under `evals/`, sorted
`attr\tdrv` lines (empty drv = no derivation; a third field `b` marks the few
attrs whose meta says broken/unsupported/insecure; `src/eval.rs`). The drv is stored
stripped of its constant `/nix/store/…​.drv` prefix/suffix, and the whole file is
zstd-compressed (default level) — together ~3× smaller (~11 MB → ~3.4 MB). An eval is bulk,
write-once, read-as-a-whole data whose *only* use is to be diffed against another
eval, so a file beats SQLite on every axis that matters here:

- **smaller** — ~3.4 MB compressed (vs ~11 MB raw, ~22 MB in SQLite: no per-row
  overhead, no `(run_id, attr)` index duplicating the data);
- **faster to diff** — both files are sorted by attr, so the changed set is a
  linear two-pointer merge over two line streams, each decompressed on its own
  thread (~12 ms, never materializing a whole file) rather than ~114k
  primary-key point-lookups (~94 ms). The cross-cutting SQL queries that would
  have justified a table never materialised (we only ever diff);
- **evictable** — when the cache grows too big, delete whole eval files for old
  commits; no `VACUUM` of a monolith. (The "millions of tiny files" failure mode
  is about a file *per attr*; one file per *eval* is ~two files per review.)

Writes are atomic — a uniquely-named temp file in the same directory (rename is
only atomic within one filesystem), then `rename` into place — so a crash can't
leave a truncated file that would poison the cache, and concurrent writers of
the same eval can't collide.

**Observations → SQLite** (`npd.sqlite`), where the append-only log actually
wants an engine: indexed lookup by `drvpath`, transactional appends, no torn
writes. It stays tiny (KBs) — this is what SQLite is *for* here. Build logs are
stored nowhere: Nix keeps them under `/nix/var/log/nix/drvs` (`nix log <drv>`,
success or failure).

**The `--tests` cache → SQLite too** (`test_pkg` / `test_drv` tables, §6). Same
reasoning inverted from evals: it's a *keyed, incremental, partial* fact (look up
a package, append new ones), not a bulk write-once map to diff — so it wants the
engine, not a file. It's small (a handful of short strings per changed package;
KBs–single-digit MB per commit, dwarfed by the eval files) and evictable by
commit, and full drv paths are stored as-is like the observation log.

```
~/.cache/nix-npd/
  npd.sqlite                    # observation log + --tests cache (tiny)
  evals/<commit>-<sys>-v<n>.tsv.zst  # attr→drv maps (zstd), one file per eval
  evals/partial/<eval>/<hash>.tsv    # in-flight shard results (§6); deleted
                                     # once the eval file is assembled
```

`nix-eval-jobs` stderr (a full Nix traceback per errored attr — megabytes over a
package set) is *not* persisted: we drain it into a small in-memory ring buffer
and surface only its tail if the eval aborts fatally.

## 5. The observation log and the build-policy predicate

Every local build appends an `Observation` (source, outcome, when, duration,
machine). The ergonomics the workflow needs are then a **pure predicate**
over that log plus substituter presence:

- marked broken/unsupported/insecure, `--build-broken` off → **skip (broken)**
  — never attempted, like nixpkgs-review; the report shows 🚧. (Checked first,
  so `--retry`/`--recheck` alone don't build it; a real fact recorded by an
  earlier `--build-broken` run still wins.)
- never observed, or forced → **build**
- a `LOCAL` success exists, `--recheck` off → **skip (ok)**
- substitutable success, `--prefer-local`/`--recheck` off → **skip (ok)**
- only failures observed, `--retry` off → **skip (fail)**
- otherwise → **build**

So the cache-bypass knobs are just fields on the policy: `recheck` (rebuild a
suspected-flaky success), `retry` (re-attempt a known failure), `prefer_local`
(don't trust a substituted success — build it here), `build_broken` (attempt
meta-blocked packages too). See `BuildPolicy::decide` in `src/model.rs`.
`--max` at the CLI is simply everything on: `--tests` + `--build-broken`.

**Staying instant when cached.** The driver loads every target's history in one
SQLite query, and only *probes the cache* for drvs it doesn't already know are
built (locally, or from a `Cache` observation a prior run recorded); those probes
run concurrently (`cache::in_cache_many`). So a changed set whose facts are all
known costs one query and no network — the whole build set is decided in
milliseconds. (Builds stay strictly behind the eval phase: they are the memory
heavyweights, and co-scheduling them with eval workers risks an OOM-killed
build being recorded as a false `Failed` fact.)
The actual build is a single batched `nix build` piped through
`nom` for the live tree, from which we recover, per drv, its outcome (built /
direct failure / dependency cascade) and duration.

**Surviving ^C.** Each outcome is recorded (and committed — every observation is
its own SQLite autocommit) the moment that drv's build activity stops, not after
the batch: nix registers a successful build's outputs *before* emitting the
activity's stop event, so output validity at stop time is the build's own
result. Interrupting the batch therefore keeps every fact observed so far —
including the failures nix itself forgets — and a re-run only re-pays for the
in-flight and never-started builds. Drvs with no build activity (blocked by a
failed dep, or valid without a build) are attributed in a post-batch sweep.

**Soundness caveats (known, accepted).** `Built` facts come from output
validity — ground truth. `Failed`/`DepFailed` facts from the post-batch sweep
are *inferences* premised on nix having finished the batch under
`--keep-going`, so the sweep skips them when nix died by a signal (exit code
absent — OOM kill, daemon restart). Residual gap: a batch that *aborts* with a
normal error exit (e.g. the daemon connection drops mid-run) is
indistinguishable by exit status from one that completed with failures, and
can then mis-attribute never-started drvs as `DepFailed`; `--retry` re-attempts
any recorded failure, so the damage is bounded and user-repairable. Also in
this class: a `Cache` fact records substitutability *at probe time* — the
remote cache deleting a path later doesn't invalidate the fact (by design,
§3), it just means nix substitutes from source instead.

## 6. Evaluation, its cache key, and the diff

**The cache key is `(commit, system, config)`, and it is not a can of worms —
provided `npd` owns the config.** What determines the attr→drv map is the
nixpkgs revision, the platform, and the nixpkgs *config* (allowlists like
`allowBroken`/`allowUnfree`/`allowUnsupportedSystem`, `permittedInsecurePackages`,
overlays, `config.allowAliases`, …). The trap is letting a user pass arbitrary
Nix as config — that isn't cleanly hashable. `npd` avoids it by **defining the
eval config itself**: one fixed allow-everything config (`EVAL_CONFIG` in
`src/eval.rs`), so the key is just `(commit, system)` plus the `npd`
eval-version tag, which is bumped whenever anything that could alter the
stored map changes — the file format, *how* `nix-eval-jobs` is invoked, or the
config itself. (An earlier design threaded a named "profile" label through the
key to leave room for several configs; with exactly one config ever defined,
the label was redundant with the version tag and was dropped.)

Caching is sound because nixpkgs evaluation is deterministic given those inputs
(drv paths are content-addressed by their inputs, stable across time and
machines); IFD is still deterministic, and impurities like `currentSystem` are
fixed by the `system` key. So "should we cache evals?" — yes, unreservedly, once
`npd` owns the config.

**Scheduling — one queue of shards.** The scheduling and failure atom is not a
whole-set eval but a **shard**: a ~400-name slice of one eval's top-level attr
names (enumerated by one cheap `builtins.attrNames` call), evaluated by its
own one-worker `nix-eval-jobs` over the same import narrowed via `listToAttrs`
— validated byte-for-byte to reproduce the monolithic walk (thunks force
per-attr in the worker, so error isolation is identical). All shards of all
pending evals share **one global queue** and one knob: the number of slots
(concurrent shard jobs), started at `min(cores, total RAM / worker cap)` —
invariants only (total RAM further capped by any cgroup memory limit the
process runs under: a container's ceiling is as much a configured promise as
the DIMMs). The dynamic part of RAM is handled by feedback, TCP-style
(AIMD), instead of measurement: a shard that aborts (in practice a worker
OOM-kill, caught by the integrity gate) is simply **requeued** while the slot
count halves; sustained success creeps it back up. Completed shards persist
immediately under `evals/partial/`, so any interruption — ^C, OOM, crash —
resumes at shard granularity; when an eval's last shard lands, its rows are
assembled into the one cached file and the partials are deleted. Small atoms
are what make everything cheap: an abort re-pays seconds (not a whole eval),
idle slots drain any eval's remaining shards (no straggler eval), and the
degenerate case — a machine that fits only one worker — is just the queue at
one slot, not a special phase. The costs: each shard job re-imports the
nixpkgs spine (a few seconds; single-digit percent of a shard's runtime at
this size), and a giant single subtree (`haskellPackages`, `linuxKernel`, the
python package sets, ~20k attrs each) is one indivisible ~minute shard that
bounds the makespan once slots ≥ total-work/max-shard (measured 1.39× over the
perfect-packing bound at 15 slots).

> Recursive splitting of those subtrees was tried and **backed out** after
> measurement: selecting attrs inside a giant package set forces that set's
> *fixpoint construction* (~15 s for `haskellPackages`) in **every** child
> shard — and once more to enumerate its names — so splitting a ~60 s subtree
> into k shards costs ~k×15 s of new work for a tail floor that can never drop
> below the construction cost. Net effect, measured on identical work: one
> fresh eval went 122 s → 191 s on a 7-slot machine, and the projected ~19 s
> tail win at 15 slots is eaten by the same overhead. Splitting only makes
> sense with a time model that knows each subtree's construction cost, or
> upstream support for sharing a constructed set across workers — revisit
> there, not with attr-count heuristics. `--eval-slots` overrides the starting slot count.

> Two earlier schemes are recorded for context. A *planner* divided measured
> available RAM into per-eval worker slots — but that snapshot lies (free RAM
> moves during a minutes-long eval, with no recovery when it did) and the
> arithmetic idled cores. A *width ladder* then retried a whole aborted eval
> at halving worker counts, with a final serialize-alone rung — it worked, but
> every rung re-paid minutes because the retry atom was the whole eval, and
> cross-eval balance was still fixed at spawn. Both dissolved into the queue:
> the ladder *is* the slot count backing off, the rung *is* the queue draining
> to one slot, and rebalancing is what a shared queue does natively.

**Eval purity vs `builtins.getEnv`.** A handful of nixpkgs packages leak the
*environment* into their derivations (drbd bakes `$SHELL` into a Makefile
patch), so two evals of the same `(commit, system)` from different shells
disagree on those drvs. npd scrubs the known offenders from the evaluator's
environment (`SHELL` removed, so `getEnv` yields `""`, matching a hermetic
eval) — the cache key stays honest without hashing the environment.

`eval(commit, system)` → `{attr: AttrEval}` via `nix-eval-jobs --meta` (cached,
pure). Each attr carries its drv plus one meta bit — marked
broken/unsupported/insecure — since meta is *not* part of the drv hash, so the
build policy and report can't recover it from the drv alone. The diff is a
set-diff on `(attr, drv_path, broken)` — a meta-only (un)marking changes no
drv but is still a review event and gets a row. (An earlier design also
sketched a *three-way* diff against the merge base, classifying each changed
attr as changed-by-this-side / by-the-other / by-both; it turned out not to
matter in practice and was dropped. The merge base survives only as the
*default base* of a report.)

**Choosing `base` and `head`.** Three ways, in `resolve_base_head`/`resolve_pr`
(`src/main.rs`):

- *Explicit* — `npd <base> <head>` resolves each revision (ref, sha, tag,
  `HEAD~1`, …) with `git rev-parse`.
- *Default* — no arguments: `head = HEAD`, `base = git merge-base master HEAD`,
  the fork point of the current branch. Cheap and offline, but it has two known
  gaps — a change branched off a *non-`master`* base (`staging`,
  `haskell-updates`, a release branch) is compared against the wrong branch, and
  the base is frozen at the fork point, so drift on the base since then is
  invisible.
- *PR* — `npd --pr N` closes both gaps by deferring to GitHub's own test-merge
  commit. GitHub publishes, on the **base repo** (so cross-fork PRs need no fork
  URL), `refs/pull/N/head` (the PR tip) and — when the PR merges cleanly —
  `refs/pull/N/merge`, a merge commit whose **first parent is the base-branch
  tip** and second parent is the PR head. So `base = merge^1`, `head = merge`
  is exactly the PR's patch applied on the *current* base branch, whatever that
  branch is — the same delta ofborg/Hydra and `nixpkgs-review pr` evaluate. This
  needs **no GitHub API and no token**: the refs come over anonymous git, unlike
  `nixpkgs-review`, which calls the REST API to learn the merge sha (and nags for
  `GITHUB_TOKEN`/`gh`). The refs are fetched into the local clone once and then
  resolved with a `rev-parse` (~0 ms), so a repeat run touches no network — which
  also *removes* the `git merge-base` walk (~0.2 s on a ~1M-commit nixpkgs) that
  otherwise dominates a fully-cached run. `--refetch` re-fetches to pick up a
  rebased PR or a moved base; a conflicted PR has no `merge` ref, and rather than
  guess a base we fail with a message pointing at `--fork-point` (PR head vs its
  merge-base with `master`, the *Default* shape). A bonus of `base = merge^1`:
  every PR reviewed in the same window shares one base commit, so their base
  evals are reused — where per-PR fork points never are.

**`--tests` — the changed set's `passthru.tests`.** Ported from
[nixpkgs-review#397](https://github.com/Mic92/nixpkgs-review/pull/397): for each
changed package, also build its `passthru.tests` (building a test derivation *is*
running it). The full-set eval never reaches these — a package's `tests` is a
plain attrset without `recurseForDerivations`, so `nix-eval-jobs` doesn't descend
into it — so `--tests` runs a **targeted second eval** over just the changed
set: a job tree `<pkg>.tests.<name>` whose per-package `tests` node is a thunk
`nix-eval-jobs` forces in a worker (so a package that fails to evaluate errors
only its own subtree, never the whole run — the same per-attr isolation the
full-set walk relies on). Each test carries its own meta-blocked bit
(broken/unsupported/insecure), and a test can be blocked while its package is not
(an x86-only `nixosTest` hung off a cross-platform package is *unsupported* on
`aarch64-linux`), so the bit is tracked per test, never inferred from the
package. Unlike a normal package, a `passthru.tests` entry is a
`nixosTest`/`vm-test-run` derivation that bypasses `check-meta`'s `commonMeta`,
so its raw `meta` has *no* computed `unsupported`/`insecure` field for `--meta`
to carry — so the tests expression **computes** the bit itself (platform support
via `lib.meta.availableOn`, insecurity via `knownVulnerabilities`) and injects it
into each test's meta (`build_tests_expr` in `src/eval.rs`). This lands the same
verdict nixpkgs-review reaches by `tryEval`-ing the outPath under a strict
config: a meta-blocked test is skipped and rendered 🚧, exactly as nixpkgs-review
lists it under "marked broken and skipped". npd evaluates the tests on **both**
sides and keeps a test only where its `(drv, broken)` pair actually differs
base→head, so the resulting rows classify (regression / fixed / new /
marked-broken / …) exactly like any other attr — a delta view, a superset of
#397's one-shot head-only build.

This eval **is cached**, but *per package* rather than as a whole-set file. A
test's drv is a pure function of `(commit, system, package-attr)` — it
does not depend on the base/head pairing — so the cache keys on the package, not
the changed set, which means a package evaluated in one review is reused in any
other at that commit. Each run looks up which changed packages are already
cached and evaluates only the misses through the **same shard scheduler as the
full-set eval** (`run_shards` in `src/eval.rs`). The misses across *every*
`(commit, system)` in the review are gathered and evaluated in **one** scheduler
run — a group per key, all shown and load-balanced together (just as the full
eval hands all its `(commit, system)` pairs to one queue), rather than one
key at a time — sliced into ~2×`eval-slots` shards so the pool stays full (a
`nixosTest` ≈ a whole NixOS system, so the AIMD memory backoff matters). It gets
the identical `done + running / total` display. Sharing the scheduler means its
concurrency logic is exercised — and kept correct — by both paths rather than
diverging. Persistence stays path-specific (§4): the full eval assembles a flat
file, `--tests` returns rows for the per-package SQLite cache. A fully-cached
re-run touches no `nix-eval-jobs` at all. Caching matters here because evaluating a test's drv
means evaluating its whole derivation graph, and a `nixosTest` in `passthru.tests`
pulls in an entire NixOS system — seconds and hundreds of MB *per test* — so a
changed set with a few dozen server/library packages is a minute of evaluation
that would otherwise repeat on every run, defeating "instant when cached". It
lives in SQLite, not a flat eval file, because the access pattern is
keyed/incremental (§4).

## 7. Cache facts — the one remote signal

The only remote fact `npd` gathers is **narinfo presence** on `cache.nixos.org`:
`HEAD /<out-hash>.narinfo` for **every output** of the drv → is this exact drv
fully substitutable? (All outputs, because the recorded fact stands for the
whole drv; substitution is per-output, so one missing output would still force
a local build.) It is drv-precise and drift-free, but **success-only** (a
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
of six states — `✅` built, `❌` failed (direct), `🚫` blocked (a dependency
failed — the transitive/cascade case, kept distinct from a direct failure), `🚧`
marked broken (meta broken/unsupported/insecure — not attempted by default; a
real build fact from a `--build-broken` run outranks the marking), `➖`
absent (no such attr on that side, or it no longer evaluates — a *known* fact,
never a `?`; in a delta view an eval breakage is visible as disappearance, so
there is no separate eval-error state), `❓` unbuilt
(has a drv, no fact yet; only under `--no-build`). A section is one `(base, head)`
state pair, and its header **is** a composable `before → after` token (one emoji
per side) — no per-row glyphs; the section a row lands in carries all the meaning.
Sections are ordered worst-delta-first, each folded in a `<details>` (an
earlier draft opened changed-state sections by default; all-collapsed read
better). Attrs that share a derivation
are collapsed onto one line (`a = b = c`, shortest attr first), like
`nixpkgs-review`'s aliases — npd gets this for free from its drvpath keying.

An `npd` run is not merely read-only: with defaults (`head` = `HEAD`, `base` =
merge-base with `master`; or the PR-derived pair under `--pr`, §6) it first
**builds both sides of the changed set** (skipping anything already known or
substitutable), so a fresh report has a real state for every row rather than a
wall of `❓`. `--no-build` opts back into pure read-only rendering.

## 9. Build order (spine first; resist features until the spine carries weight)

The spine is implemented (✓).

1. ✓ cached `eval(commit, system)` → attr→drv map (`nix-eval-jobs`), evals run
   in parallel with an OOM-recovery ladder (§6).
2. ✓ the two-way diff (base/head chosen explicitly, from the merge-base with
   `master`, or from a PR's GitHub test-merge commit under `--pr`; §6).
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

**Known gotcha (root-caused) — `nix-eval-jobs` restarts its worker after every
job on macOS.** The ~100× darwin slowdown (measured ~1.5 attrs/s on an
`aarch64-darwin` VM vs ~155 attrs/s on `aarch64-linux`, same hardware) is a
units bug in `nix-eval-jobs`' worker-restart check (`shouldRestart`,
`src/worker.cc`): it compares `getrusage`'s `ru_maxrss` against
`--max-memory-size` (MiB) × 1024, which is correct on Linux (`ru_maxrss` in
KiB) but off by 1024× on macOS (`ru_maxrss` in **bytes**). The effective cap
becomes `--max-memory-size` *KiB*, every worker trips it after its first job,
and each job pays a fork + full nixpkgs re-import (~0.6 s each; also why "huge"
MB values didn't help — 999999 MB still reads as ~1 GB). It was never a GC or
eval-engine problem: with the cap compensated ×1024, the same darwin VM
evaluates *faster* than the Linux VM (7671 vs 5134 attrs/30 s, one worker).
Reported as [nix-eval-jobs#425](https://github.com/NixOS/nix-eval-jobs/issues/425)
and fixed by [nix-eval-jobs#426](https://github.com/NixOS/nix-eval-jobs/pull/426)
(merged 2026-07-10). npd works around it by passing `--max-memory-size` ×1024 on
macOS (see `stream_jobs` in `src/eval.rs`); drop that once the fix reaches the
`nix-eval-jobs` npd runs.

## 10. Resolved questions

Recorded for context:

- *Eval cache key* → `(commit, system)` with an eval-version tag standing in
  for the fixed config; not a can of worms because `npd` owns the config (§6).
- *Concurrency* → not handled. One machine is the driver and keeps its store
  local; multiple drivers keep independent stores, exactly as the Nix store
  already works. The append-only design stays friendly to revisiting this.
- *Cache facts lifetime* → append-only observations, no eviction/TTL. A `Cache`
  observation records the drvpath, so staleness can't affect correctness (§3).
- *Remote facts* → narinfo on `cache.nixos.org` only; Hydra was dropped (§7).
- *Storage* → SQLite (`npd.sqlite`) under `dirs::cache_dir()/nix-npd`; all re-derivable cache (§4).
