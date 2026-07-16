# npd

A persistent **fact store** for iterating on nixpkgs changes across a set of
long-lived build machines, keyed on the identity of build *recipes*
(derivation paths).

`npd` reviews a nixpkgs PR — evaluate a `base → head` change, build the changed
set, render a report — the same core job as
[`nixpkgs-review`](https://github.com/Mic92/nixpkgs-review). What sets it apart
is what it *keeps*: nixpkgs-review reviews one PR, one-shot, and throws the
workspace away, whereas `npd` is built around a durable, `drvpath`-keyed **fact
store**, so across a loop of related reviews over days it never repeats work
whose answer it already knows. (And even on a single *cold* review — nothing
cached — it holds its own with nixpkgs-review on the pre-build eval/diff path,
often beating it once it can use the machine's cores.)

The Nix store + substituters already remember **successful** builds. What Nix
throws away is everything else `npd` cares about:

- **local failures** — Nix retries a failed build every time; `npd` remembers a
  failed drv (direct failure vs. dependency cascade) so your loop doesn't repeat it.
- **eval diffs** — the attr→drv map of a revision is expensive and uncached.
- **reports** — human-readable Markdown over all of the above.

The one remote fact `npd` consults is `cache.nixos.org` (is this exact drv already
built and substitutable?). So `npd` is a thin **fact store + policy layer over
`nix-eval-jobs` and `nix build`**. See [`DESIGN.md`](DESIGN.md).

## Status

Rust (edition 2024, à la [`npc`](https://github.com/samestep/npc)). `npd` is a
single command: evaluate a `base → head` change, build whatever the changed set
needs, and render the report — **instant when the result is already known**.

```
npd [--base <rev>] [--head <rev>]
npd --pr <N>
```

With no arguments, `head` = `HEAD` and the base is the `master` tip; the report
compares that base against the head **merged onto it** (a synthetic merge — the
same shape a PR's test-merge gives), so base drift is visible. `--pr <N>` is
shorthand for the PR's head merged onto its base branch (reusing GitHub's
test-merge commit; no API/token). `--no-merge` opts back into the older
`merge-base → head` fork-point diff (offline, but blind to base drift).
It **builds whatever the states need** first (both sides of the changed set,
skipping anything already known, substitutable, or meta-blocked
(broken/unsupported/insecure) — the last reported as ⏩ **skipped**, npd's name
for what nixpkgs-review skips), then groups the result by its `before → after`
delta (regression / blocked-by-a-regression / newly-skipped / fixed / dropped /
…), folded, with drv-sharing attrs collapsed (`a = b = c`). Flags: `--no-build`
(render from existing facts only), `--recheck` / `--retry` / `--prefer-local`
(build-policy knobs), `--no-tests` (skip each changed package's
`passthru.tests`, built on both sides by default — ported from
[nixpkgs-review#397](https://github.com/Mic92/nixpkgs-review/pull/397)),
`--no-skip` (build the meta-blocked packages npd otherwise skips), `--max`
(everything on: implies `--no-skip`; tests are on by default),
`--system` (repeatable), `--nixpkgs`, and sizing knobs for the parallel
evaluator (`--eval-slots`, `--worker-mem-mb`). Under the hood: evals cached
as flat per-commit files (diffed by a streaming linear merge), a tiny SQLite
observation log, evaluation as one queue of `nix-eval-jobs` shard jobs
(a shard that dies — usually a worker OOM — just requeues while the slot
count backs off; interrupted evals resume at shard granularity), and one
batched `nom` build with concurrent cache probing.

## Development

Toolchain comes from the flake (like `npc`): `direnv allow`, or `nix develop`.

```sh
nix develop --command cargo test    # unit tests (an ignored end-to-end test needs real nix)
nix develop --command cargo run -- --help
```
