# npd design

## 1. Purpose and scope

`npd` supports a **durable, iterative** nixpkgs workflow on a fixed set of
long-lived build machines with plenty of disk. It exists to make these cheap:

- evaluate a revision → the set of `attr → derivation` on each platform;
- diff two revisions (and, three-way, their merge base) to a set of changed attrs;
- learn what Hydra knows about a derivation or job;
- build derivations locally;
- render human-readable Markdown reports from all of the above;

…while **never repeating expensive work whose answer is already known**, and
while making it ergonomic to *deliberately* ignore the cache (build locally
instead of substituting; rebuild a success you suspect is flaky; skip a failure
you expect to just repeat).

### What `npd` is not

- Not a `nixpkgs-review` replacement. The build-result **classifier / comparison
  report** (the 3×3 base×change grid, the composed status token, `narinfo`/drift
  logic) is a *presentation* concern that belongs upstream in nixpkgs-review as a
  self-contained feature; `npd` reuses that rendering rather than owning it.
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

| fact | key | discipline |
| --- | --- | --- |
| **eval** — attr→drv map + meta | `(commit, system, config)` | **pure** → cache forever, never invalidate |
| **observation** — one build/lookup event | `drvpath` (or output path for `Cache`) | **append-only log** — never overwrite |

An eval at a fixed `(commit, system, config)` is deterministic, so its result is
valid forever. Everything else is an **observation**: a single event, from some
`Source` — a `Local` build we ran, a `HydraJob` build record, or `Cache`
(narinfo) presence — stamped with `when`. We append and never discard, which is
what makes flakiness representable (multiple observations of one drv with
differing outcomes).

**Hydra facts are observations too** — not a separate mutable store. A Hydra
build record keyed by `build_id` is itself immutable; "what is the latest build
of job X" and "is output H in the cache right now" are just observations we make
at time `when`. So there is no eviction and no TTL. Crucially, because a Hydra
observation already records the *drvpath*, staleness never affects
**correctness** — only whether we bother to re-fetch. So we start with
**manually-triggered** Hydra fetching and no freshness threshold at all; an
auto-refresh policy can come later if it earns its keep. This keeps full history
(a job that went green → red → green is visible) and unifies local and remote
facts under one log.

## 4. Storage

Everything `npd` stores is re-derivable, so it lives under
`dirs::cache_dir()/nix-npd` (i.e. `~/.cache/nix-npd`), like `npc`. The gcroots below are
the one thing that must survive Nix GC while it exists, but the *records* are all
cache: losing them costs re-evaluation / re-building, not correctness.

**First, a non-problem to dispel:** we never need to build an in-memory reverse
index at startup. We only ever look facts up by keys we already hold — an attr
or job name, an output hash, a drvpath — and **the eval fact is itself the join**
between the drvpath world and Hydra's name-/output-keyed world (given an eval,
`attr ⇄ drv ⇄ outputs ⇄ job`). So per-key access is direct regardless of backend.

**Backend: SQLite** (`npd.sqlite`, one file) for both eval maps and the
observation log; **files** only for build logs (naturally large blobs). Schema
lives in `src/store.rs`. Why SQLite over a pile of JSON files (a full-set eval
is ~114k rows / ~27 MB of JSON, ~85% redundant — it compresses ~6.5×):

- indexes give O(log n) lookup by `drvpath` / output hash / `(job, system)` with
  no manual index files, and a normalized table captures that redundancy natively;
- it avoids the millions-of-tiny-files failure mode (inode pressure, slow
  `readdir`, directory sharding) that a fact-per-file scheme hits over time;
- transactional appends avoid torn writes;
- the two-way / three-way eval diff and cross-cutting queries ("everything that
  fails locally but is green on Hydra", "all flaky drvs") are one SQL query
  rather than loading and parsing multiple 27 MB blobs.

`existence` is not persisted — it is recomputed from `drv_path` + the meta flags
on load, so there is one source of truth for that mapping.

```
~/.cache/nix-npd/
  npd.sqlite                    # evals + observation log
  logs/<drv-hash>/<obs-id>.log  # build logs referenced by observations
  gcroots/<drv-hash>-<output>   # nix gcroots for outputs we choose to keep
```

`<drv-hash>` is the 32-char hash component of the drvpath. gcroots are
mandatory for anything we want to survive `nix-collect-garbage`.

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
(don't trust a substituted/Hydra success — build it here). See
`BuildPolicy::decide` in `src/model.rs`.

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

## 7. Hydra facts — best-effort, tiered

There is **no reverse index** from a store path to a Hydra job on
hydra.nixos.org (search is name-keyed and 500s on paths; no `/store-path`
endpoint). So Hydra answers are best-effort, cheapest-first:

1. **narinfo** `HEAD cache.nixos.org/<out-hash>.narinfo` — drv-precise, drift-free,
   but **success-only** (404 conflates never-built / failed / GC-evicted). Cheap.
2. **forward job** `/job/<jobset>/<attr>.<system>/latest` — status + logs, but for
   the job's *latest* drv, which may differ from ours (**drift**). Medium.
3. **local rebuild** — ground truth; disambiguates narinfo's 404. Expensive.

Because our own eval already yields the base revision's *exact* output paths,
narinfo on **those** paths is drv-precise, and disagreeing with the forward-job
verdict is a **drift detector** (Hydra's green is a different derivation than
ours — the failure mode that first motivated this whole line of work). Hydra's
`isCachedBuild` flag / build duration further tells us whether a Hydra verdict is
a genuine run or a reused cached result.

Every Hydra lookup is recorded as an `Observation` (`Source::HydraJob` or
`Source::Cache`) in the same append-only log (§3), so a Hydra verdict is stored
and reasoned about identically to a local build. Fetching is **on demand** via
the `hydra` subcommand for now (§9); whether/when to prefetch is deferred.

Upstream opportunity (separate): Hydra already indexes `BuildOutputs.path`
(hash) and `Builds.drvpath` (btree + trigram); its `/search` merely uses a
substring `ilike` that can't use those indexes and times out. A small PR adding
an exact `drvpath`/`path` lookup would give a real reverse endpoint (surfacing
failures + cached flags), which `npd` would prefer over narinfo when available.

## 8. Reports

Markdown, reusing the nixpkgs-review comparison classifier: group each attr by
its **delta** (regression / fixed / dropped / added / pre-existing / unchanged /
uncertain) for triage, and render a **composed token** per row (`before → after`,
tagged with source and confidence/drift) so no information is lost. Cascades
(`dependency failed`) are separated from direct failures and attributed to their
root.

## 9. Build order (spine first; resist features until the spine carries weight)

The spine is implemented (✓); what remains are refinements.

1. ✓ cached `eval(commit, system)` → attr→drv map (`nix-eval-jobs`).
2. ✓ two-way diff, then the three-way (merge-base) diff.
3. ✓ the drvpath-keyed observation store + `BuildPolicy` + a local build driver
   that consults/appends it and manages gcroots.
4. ✓ Hydra facts (narinfo → forward job → drift), recorded as observations.
5. ✓ Markdown report classifying the changed set from the observation log.

Open refinements: `substitutable` build pre-skip (batch validity/narinfo so we
don't invoke `nix build` on already-available drvs); `DepFailed`/cascade
detection (the 0-byte-log signal) so a dependency failure isn't counted as a
direct one; `Local`-vs-`Cache` build fidelity (a dry-run probe to tell a
from-source build from a substitution); parallel builds; remote-builder fan-out.

## 10. Open questions

- The report classifier's eventual home (§8) — revisit when we get to reports.

Resolved earlier and recorded for context:

- *Eval cache key* → `(commit, system, profile)` with an eval-version tag; not a
  can of worms because `npd` owns the config (§6).
- *Concurrency* → not handled. One machine is the driver and keeps its store
  local; multiple drivers keep independent stores, exactly as the Nix store
  already works. The append-only design stays friendly to revisiting this.
- *Hydra facts lifetime* → append-only observations, no eviction/TTL. Fetching is
  manual for now and there is no freshness threshold, since a Hydra observation
  records the drvpath so staleness can't affect correctness (§3).
- *Storage* → SQLite (`npd.sqlite`) under `dirs::cache_dir()/nix-npd`; all re-derivable cache (§4).
