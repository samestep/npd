# npd

A persistent **fact store** for iterating on nixpkgs changes across a set of
long-lived build machines, keyed on the identity of build *recipes*
(derivation paths).

`npd` is **not** a re-implementation of [`nixpkgs-review`](https://github.com/Mic92/nixpkgs-review).
nixpkgs-review reviews one PR, one-shot, and throws the workspace away. `npd`
optimizes for the opposite: a durable loop where you evaluate, build, and
re-build the same and related derivations many times over days, and never want
to repeat work you already know the answer to.

The Nix store + substituters already remember **successful** builds. What Nix
throws away is everything else `npd` cares about:

- **local failures** — Nix retries a failed build every time; Hydra solved this
  with its global `FailedPaths` table. `npd` keeps the same memory for your loop.
- **eval diffs** — the attr→drv map of a revision is expensive and uncached.
- **Hydra facts** — job status, `narinfo` presence, and derivation *drift*.
- **reports** — human-readable Markdown over all of the above.

So `npd` is a thin **fact store + policy layer over `nix-eval-jobs` and
`nix build`**, not a fork of a review tool. See [`DESIGN.md`](DESIGN.md).

## Status

Rust (edition 2024, à la [`npc`](https://github.com/samestep/npc)). The spine is
implemented end-to-end:

- `npd eval <commit>` — cached attr→drv map (SQLite; streamed `nix-eval-jobs`).
- `npd diff <base> <head> [--three-way]` — changed/added/removed, with merge-base
  attribution.
- `npd build <commit> <attrs…>` / `--changed <base>` — observation-backed build
  driver (remembers successes *and failures*), gc-roots outputs, `--dry-run`.
- `npd hydra <commit> <attrs…>` — records `Cache` (narinfo, drv-precise) and
  `HydraJob` (forward, drift-checked) observations.
- `npd report <base> <head>` — classifies the changed set (regression / fixed /
  pre-existing / dropped / …) from the observation log.

Refinements still open: `substitutable` build pre-skip, `DepFailed`/cascade
detection, parallel builds. See `DESIGN.md`.

## Development

Toolchain comes from the flake (like `npc`): `direnv allow`, or `nix develop`.

```sh
nix develop --command cargo test    # run the model tests
nix develop --command cargo run -- --help
```
