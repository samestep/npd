# `npd`

For example, `npd --pr 542936`:

> ## [`npd`](/) report: [`dce745c`](https://github.com/NixOS/nixpkgs/commit/dce745ce155ce35a8121ee7280e7ae53559cead3) → [`5f96e8f`](https://github.com/NixOS/nixpkgs/commit/5f96e8fa57f8703504fe54b642bfcb4264aa9d4d)
>
> ```sh
> npd --base dce745ce155ce35a8121ee7280e7ae53559cead3 --head 840dbf16cf78ddd86383d55a3beefa44df86cfd9 --patch 840dbf16cf78ddd86383d55a3beefa44df86cfd9...5f96e8fa57f8703504fe54b642bfcb4264aa9d4d -s x86_64-linux
> ```
>
> ### `x86_64-linux`
>
> <details><summary>✅ → ❌ · <b>1 regression</b> — build on the base, fail here</summary>
>
> - `coqPackages.graph-theory`
> </details>
>
> <details><summary>✅ → 🚫 · <b>5 blocked packages</b> (8 attrs) — build on the base, a dependency fails here</summary>
>
> - `coqPackages.mathcomp-analysis-stdlib` = `rocqPackages.mathcomp-analysis-stdlib`
> - `coqPackages.mathcomp-analysis` = `rocqPackages.mathcomp-analysis`
> - `coqPackages.mathcomp-experimental-reals` = `rocqPackages.mathcomp-experimental-reals`
> - `coqPackages.mathcomp-infotheo`
> - `coqPackages.ssprove`
> </details>
>
> <details><summary>✅ → ➖ · <b>1 dropped package</b> — build on the base, gone here</summary>
>
> - `rocqPackages.mathcomp-real-closed`
> </details>
>
> <details><summary>✅ → ✅ · <b>13 unchanged packages</b> (17 attrs) — build on the base and here</summary>
>
> - `coqPackages.coqeal`
> - `coqPackages.fourcolor`
> - `coqPackages.gaia`
> - `coqPackages.libvalidsdp`
> - `coqPackages.mathcomp-classical` = `rocqPackages.mathcomp-classical`
> - `coqPackages.mathcomp-finmap` = `rocqPackages.mathcomp-finmap`
> - `coqPackages.mathcomp-real-closed`
> - `coqPackages.mathcomp-reals-stdlib` = `rocqPackages.mathcomp-reals-stdlib`
> - `coqPackages.mathcomp-reals` = `rocqPackages.mathcomp-reals`
> - `coqPackages.mathcomp-tarjan`
> - `coqPackages.multinomials`
> - `coqPackages.odd-order`
> - `coqPackages.validsdp`
> </details>
