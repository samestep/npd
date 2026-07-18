//! Render a Markdown report of a change (`base → head`).
//!
//! Each attr in the changed set has a *state* on each side — reduced from the
//! observation log (§8) — and the report groups attrs by the `(base, head)`
//! state pair. The section header *is* a composable `before → after` token
//! (one emoji per side); no per-row glyphs. Attrs that share a derivation are
//! collapsed onto one line (`a = b = c`), like `nixpkgs-review`'s aliases.

use std::collections::{BTreeMap, HashMap};

use crate::model::{Observation, Outcome, Source};

/// One side's build state, reduced from a drv's observations (or its absence).
/// `Ord` (declaration order) only tie-breaks section order among pairs sharing
/// a [`priority`], so the report is deterministic.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum State {
    /// Output valid — built locally, or substitutable from the cache.
    Built,
    /// Its own build failed (a direct failure).
    Failed,
    /// A dependency failed, so this never ran (a transitive/cascade failure).
    Blocked,
    /// Meta-blocked (broken/unsupported/insecure) — not attempted by default,
    /// nixpkgs-review's "skipped" (its meta-blocked subset; a *missing* attr is
    /// `Absent`, not this). `--no-skip` builds it anyway, and any real build
    /// fact then outranks this state.
    Skipped,
    /// No derivation on this side (the attr doesn't exist there).
    Absent,
    /// Has a derivation but no build fact yet. Builds always run, so this is
    /// only the build phase's accepted gap (§5): a target nix never reached,
    /// with nothing verifiably failing in its closure, left unrecorded to be
    /// re-attempted next run.
    Unknown,
}

impl State {
    /// Every state, in enum-declaration order — used to render the symbol legend.
    const ALL: [State; 6] = [
        State::Built,
        State::Failed,
        State::Blocked,
        State::Skipped,
        State::Absent,
        State::Unknown,
    ];

    fn glyph(self) -> &'static str {
        match self {
            State::Built => "✅",
            State::Failed => "❌",
            State::Blocked => "🚫",
            State::Skipped => "⏩",
            State::Absent => "➖",
            State::Unknown => "❔",
        }
    }

    /// Legend gloss for this state's glyph (see [`render`]'s symbol legend).
    fn label(self) -> &'static str {
        match self {
            State::Built => "successfully built",
            State::Failed => "failed to build",
            State::Blocked => "dependency failed to build",
            State::Skipped => "didn't try to build",
            State::Absent => "doesn't exist",
            State::Unknown => "couldn't try to build",
        }
    }

    /// Goodness on the build-outcome axis, higher = better: `✅` built, `➖`
    /// absent ("new" on the base side / "gone" on the head side) just under it,
    /// then `⏩` skipped, `🚫` blocked, `❌` failed. `❔` unbuilt is off the axis
    /// — no fact to compare against — so it has no goodness; [`priority`] tiers
    /// it out before this is ever reached.
    fn goodness(self) -> i32 {
        match self {
            State::Built => 4,
            State::Absent => 3,
            State::Skipped => 2,
            State::Blocked => 1,
            State::Failed => 0,
            State::Unknown => {
                unreachable!("Unknown is off the goodness axis; priority() tiers it out")
            }
        }
    }
}

/// Reduce a side (its optional drv + meta-blocked bit + that drv's
/// observations) to a state.
///
/// Local observations are ground truth and win over everything; a local success
/// beats a local failure (it *can* build). A direct failure outranks a
/// dependency failure (it's the more specific fact about this drv). Being
/// meta-blocked (`Skipped`) only shows when no build fact exists — a package
/// built anyway (`--no-skip`) reports its real outcome.
pub fn side_state(drv: &Option<String>, skipped: bool, obs: &[Observation]) -> State {
    if drv.is_none() {
        return State::Absent;
    }
    let has = |src: Source, out: Outcome| obs.iter().any(|o| o.source == src && o.outcome == out);
    if has(Source::Local, Outcome::Built) {
        State::Built
    } else if has(Source::Local, Outcome::Failed) {
        State::Failed
    } else if has(Source::Local, Outcome::DepFailed) {
        State::Blocked
    } else if has(Source::Cache, Outcome::Built) {
        State::Built
    } else if skipped {
        State::Skipped
    } else {
        State::Unknown
    }
}

/// One changed attr for a system: its drv on each side and each side's state.
pub struct Entry {
    pub attr: String,
    pub base_drv: Option<String>,
    pub head_drv: Option<String>,
    pub base: State,
    pub head: State,
}

/// Sort key for a `(base, head)` section, **worst-delta-first** (DESIGN §8).
///
/// A section with a fact on both sides sorts by the signed delta
/// `goodness(head) − goodness(base)` ascending, so the steepest regression
/// (`✅→❌`) leads and every improvement trails; equal deltas break by a worse
/// current state (lower `goodness(head)`). A side still `Unknown` has no
/// measured delta, so the pair drops to a final tier. `render` appends
/// `(base, head)` as a last tie-break, making the whole order total — so the
/// report is deterministic.
fn priority(base: State, head: State) -> (u8, i32, i32) {
    if base == State::Unknown || head == State::Unknown {
        return (1, 0, 0);
    }
    (0, head.goodness() - base.goodness(), head.goodness())
}

/// Render one section: its `before → after` header, then one bullet per group
/// of attrs sharing a derivation (`a = b = c`, shortest attr first).
fn render_section(base: State, head: State, entries: &[&Entry]) -> String {
    // Group attrs by their (base, head) drv pair — same pair ⇒ same build.
    let mut by_drv: BTreeMap<(Option<String>, Option<String>), Vec<String>> = BTreeMap::new();
    for e in entries {
        by_drv
            .entry((e.base_drv.clone(), e.head_drv.clone()))
            .or_default()
            .push(e.attr.clone());
    }
    let groups = by_drv.len();
    let attrs_total = entries.len();

    // The bold count is the attr total; pluralize by it.
    let plural = if attrs_total == 1 { "" } else { "s" };
    // Note the distinct-derivation count too, but only when grouping collapsed rows.
    let note = if attrs_total != groups {
        format!(" ({groups} unique)")
    } else {
        String::new()
    };

    // Sections carry their own separator *before* them (a leading blank line),
    // so the gaps fall between sections and none trails the last one.
    let mut s = format!(
        "\n<details><summary>{} → {} · <b>{attrs_total} package{plural}</b>{note}</summary>\n\n",
        base.glyph(),
        head.glyph(),
    );
    // One line per drv-group; within a line, shortest attr first; lines sorted.
    let mut lines: Vec<String> = by_drv
        .values()
        .map(|attrs| {
            let mut a = attrs.clone();
            a.sort_by(|x, y| x.len().cmp(&y.len()).then_with(|| x.cmp(y)));
            a.iter()
                .map(|x| format!("`{x}`"))
                .collect::<Vec<_>>()
                .join(" = ")
        })
        .collect();
    lines.sort();
    for line in lines {
        s.push_str(&format!("- {line}\n"));
    }
    s.push_str("</details>\n");
    s
}

/// The longest run of consecutive backticks in `s`.
fn longest_backtick_run(s: &str) -> usize {
    let (mut max, mut cur) = (0, 0);
    for c in s.chars() {
        cur = if c == '`' { cur + 1 } else { 0 };
        max = max.max(cur);
    }
    max
}

/// Render the per-system entries to Markdown, grouped into `before → after`
/// sections ordered worst-delta-first. `command` is the shell reproduction of
/// this exact changeset (see `repro_command`), tucked under the heading inside a
/// folded <details> alongside a legend of the glyphs (DESIGN §8).
pub fn render(
    base: &str,
    head: &str,
    command: &str,
    per_system: &[(String, Vec<Entry>)],
) -> String {
    // Fence with more backticks than any run inside the command, so a working-
    // tree reproduction whose embedded diff touches a Markdown file (its own
    // ``` fences and all) can't close the block early.
    let fence = "`".repeat(longest_backtick_run(command).max(2) + 1);
    // Bare commit hashes (no code span) so GitHub auto-links them as short SHAs.
    // `npd` links to the exact source tree this binary was built from (§8).
    let url = crate::URL;
    let mut out = format!("## [`npd`]({url}) · {base} → {head}\n\n");
    // The reproduction command and the glyph legend each fold away behind a
    // <details>, keeping the heading close to the per-system sections below.
    out.push_str("<details><summary>Expand this for a reproducible command.</summary>\n\n");
    out.push_str(&format!("{fence}sh\n{command}\n{fence}\n"));
    out.push_str("</details>\n\n");
    out.push_str("<details><summary>Expand this for a legend of all symbols below.</summary>\n\n");
    for state in State::ALL {
        out.push_str(&format!("- {} = {}\n", state.glyph(), state.label()));
    }
    out.push_str("</details>\n");
    for (system, entries) in per_system {
        out.push_str(&format!("\n### `{system}`\n"));
        if entries.is_empty() {
            out.push_str("\n_No changed attrs._\n");
            continue;
        }
        // Bucket by (base, head) state, then emit buckets in priority order.
        let mut buckets: HashMap<(State, State), Vec<&Entry>> = HashMap::new();
        for e in entries {
            buckets.entry((e.base, e.head)).or_default().push(e);
        }
        let mut keys: Vec<(State, State)> = buckets.keys().copied().collect();
        // The state pair tie-breaks equal priorities (the whole Unknown tier
        // shares one key), keeping the output deterministic.
        keys.sort_by_key(|&(b, h)| (priority(b, h), b, h));
        for (b, h) in keys {
            out.push_str(&render_section(b, h, &buckets[&(b, h)]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(source: Source, outcome: Outcome) -> Observation {
        Observation {
            drv_path: "/nix/store/x.drv".into(),
            source,
            outcome,
            when: 0,
            blocker: Vec::new(),
        }
    }

    #[test]
    fn state_reduction() {
        // No drv on a side is Absent, not Unknown.
        assert_eq!(side_state(&None, false, &[]), State::Absent);
        // A drv with no facts is Unknown (distinct from Absent).
        let d = Some("/nix/store/x.drv".to_string());
        assert_eq!(side_state(&d, false, &[]), State::Unknown);
        // Direct vs transitive failures are distinguished.
        assert_eq!(
            side_state(&d, false, &[obs(Source::Local, Outcome::Failed)]),
            State::Failed
        );
        assert_eq!(
            side_state(&d, false, &[obs(Source::Local, Outcome::DepFailed)]),
            State::Blocked
        );
        // Cache success reads as Built; a local build wins over it.
        assert_eq!(
            side_state(&d, false, &[obs(Source::Cache, Outcome::Built)]),
            State::Built
        );
        let s = side_state(
            &d,
            false,
            &[
                obs(Source::Cache, Outcome::Built),
                obs(Source::Local, Outcome::Failed),
            ],
        );
        assert_eq!(s, State::Failed);
        // Meta-blocked with no facts is Skipped; a real fact (a --no-skip
        // run's build or failure) outranks the marking. No drv is still Absent.
        assert_eq!(side_state(&d, true, &[]), State::Skipped);
        assert_eq!(
            side_state(&d, true, &[obs(Source::Local, Outcome::Built)]),
            State::Built
        );
        assert_eq!(
            side_state(&d, true, &[obs(Source::Local, Outcome::Failed)]),
            State::Failed
        );
        assert_eq!(side_state(&None, true, &[]), State::Absent);
    }

    #[test]
    fn reproduction_fence_outgrows_embedded_backticks() {
        // A working-tree reproduction embeds its diff, which can contain a ```
        // run (editing a Markdown file). The fence must be longer so the report
        // block doesn't close early.
        let cmd = "git apply --cached <<'PATCH'\n+```sh hi\nPATCH";
        let out = render("b", "h", cmd, &[]);
        assert!(out.contains("\n````sh\n"), "{out}");
        // The block closes on its own oversized fence, then the <details> wrapping it.
        assert!(out.contains("\n````\n</details>\n"), "{out}");
        // The common (no-backtick) command still gets a plain triple fence.
        let out = render("b", "h", "npd --base a --head b", &[]);
        assert!(out.contains("\n```sh\n"), "{out}");
    }

    #[test]
    fn priority_orders_worst_delta_first() {
        use State::{Absent, Blocked, Built, Failed, Skipped, Unknown};
        let mut pairs: Vec<(State, State)> = State::ALL
            .iter()
            .flat_map(|&b| State::ALL.iter().map(move |&h| (b, h)))
            .collect();

        // The full sort key (priority + the (base, head) tie-break render uses)
        // is a total order: every pair gets a distinct slot, so section order is
        // deterministic.
        let mut seen = std::collections::HashSet::new();
        for &(b, h) in &pairs {
            assert!(
                seen.insert((priority(b, h), b, h)),
                "duplicate slot {b:?}→{h:?}"
            );
        }
        assert_eq!(seen.len(), 36);

        pairs.sort_by_key(|&(b, h)| (priority(b, h), b, h));
        let at = |p| pairs.iter().position(|&x| x == p).unwrap();
        // The steepest fall leads; a regression outranks unchanged, which
        // outranks any improvement.
        assert_eq!(pairs[0], (Built, Failed));
        assert!(at((Built, Skipped)) < at((Built, Built)));
        assert!(at((Built, Built)) < at((Failed, Built)));
        // Equal deltas break by a worse current state: Δ=-2 lands
        // ⏩→❌ before ➖→🚫 before ✅→⏩.
        assert!(at((Skipped, Failed)) < at((Absent, Blocked)));
        assert!(at((Absent, Blocked)) < at((Built, Skipped)));
        // No measured delta (either side Unknown) sinks to a final contiguous tier.
        let first_unknown = pairs
            .iter()
            .position(|&(b, h)| b == Unknown || h == Unknown)
            .unwrap();
        assert!(
            pairs[first_unknown..]
                .iter()
                .all(|&(b, h)| b == Unknown || h == Unknown)
        );
    }

    fn entry(attr: &str, base: State, head: State, bd: Option<&str>, hd: Option<&str>) -> Entry {
        Entry {
            attr: attr.into(),
            base_drv: bd.map(str::to_string),
            head_drv: hd.map(str::to_string),
            base,
            head,
        }
    }

    #[test]
    fn render_sections_tokens_grouping_and_folding() {
        let entries = vec![
            // a regression
            entry(
                "pkgA",
                State::Built,
                State::Failed,
                Some("/b/a.drv"),
                Some("/h/a.drv"),
            ),
            // two distinct blocked drvs, transitive glyph 🚫
            entry(
                "dep1",
                State::Built,
                State::Blocked,
                Some("/b/d1"),
                Some("/h/d1"),
            ),
            entry(
                "dep2",
                State::Built,
                State::Blocked,
                Some("/b/d2"),
                Some("/h/d2"),
            ),
            // newly skipped (meta), distinct from dep-blocked
            entry(
                "brk",
                State::Built,
                State::Skipped,
                Some("/b/k"),
                Some("/h/k"),
            ),
            // two attrs sharing one drv, unchanged (grouped onto one line)
            entry(
                "z.foo",
                State::Built,
                State::Built,
                Some("/b/f"),
                Some("/h/f"),
            ),
            entry(
                "foo",
                State::Built,
                State::Built,
                Some("/b/f"),
                Some("/h/f"),
            ),
        ];
        let out = render(
            "base",
            "head",
            "npd --base base --head head",
            &[("aarch64-linux".into(), entries)],
        );

        // The reproduction command sits in a code block behind a folded <details>.
        assert!(
            out.contains(
                "<details><summary>Expand this for a reproducible command.</summary>\n\n\
                 ```sh\nnpd --base base --head head\n```\n</details>\n"
            ),
            "{out}"
        );
        // The glyph legend is a second folded <details>, one bullet per state.
        assert!(
            out.contains(
                "<details><summary>Expand this for a legend of all symbols below.</summary>"
            ),
            "{out}"
        );
        assert!(out.contains("- ✅ = successfully built\n"), "{out}");
        assert!(out.contains("- ❔ = couldn't try to build\n"), "{out}");

        // Composable glyph tokens with a plain package count; the transitive
        // distinction shows through 🚫.
        assert!(out.contains("✅ → ❌ · <b>1 package</b>"), "{out}");
        assert!(out.contains("✅ → 🚫 · <b>2 packages</b>"), "{out}");
        assert!(out.contains("✅ → ⏩ · <b>1 package</b>"), "{out}");
        // Grouping: shared drv collapses to one equals-joined line, shortest first.
        assert!(out.contains("- `foo` = `z.foo`"), "{out}");
        // Two attrs, one derivation: the total counts, the distinct count notes.
        assert!(
            out.contains("✅ → ✅ · <b>2 packages</b> (1 unique)"),
            "{out}"
        );
        // All sections are folded closed.
        assert!(out.contains("<details><summary>✅ → ❌"), "{out}");
        assert!(out.contains("<details><summary>✅ → ✅"), "{out}");
        assert!(!out.contains("<details open>"), "{out}");
        // Ordering: regression before blocked before newly-skipped before unchanged.
        let reg = out.find("→ ❌").unwrap();
        let blk = out.find("→ 🚫").unwrap();
        let brk = out.find("→ ⏩").unwrap();
        let unch = out.find("✅ → ✅").unwrap();
        assert!(reg < blk && blk < brk && brk < unch, "{out}");
    }
}
