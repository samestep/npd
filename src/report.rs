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
/// a [`cell`] priority, so the report is deterministic.
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
    /// Has a derivation but no build fact yet (only reachable with `--no-build`).
    Unknown,
}

impl State {
    fn glyph(self) -> &'static str {
        match self {
            State::Built => "✅",
            State::Failed => "❌",
            State::Blocked => "🚫",
            State::Skipped => "⏩",
            State::Absent => "➖",
            State::Unknown => "❓",
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

/// The section a `(base, head)` pair belongs to: an emission-priority index
/// (lower = worse / more actionable, emitted first), a count noun, and a
/// phrase. Exhaustive over all 36 pairs — no catch-all — so adding a [`State`]
/// forces every new pair to be placed deliberately.
fn cell(base: State, head: State) -> (usize, &'static str, &'static str) {
    use State::{Absent, Blocked, Built, Failed, Skipped, Unknown};
    // Nouns are singular count-nouns (pluralized with a trailing "s" by the
    // renderer), so the phrase, not the noun, carries the before→after detail.
    // "Skipped" (meta-blocked: broken/unsupported/insecure) is nixpkgs-review's
    // term; rows failing/building *from* Skipped are only reachable via --no-skip.
    match (base, head) {
        (Built, Failed) => (0, "regression", "build on the base, fail here"),
        (Built, Blocked) => (
            1,
            "blocked package",
            "build on the base, a dependency fails here",
        ),
        (Absent, Failed) => (2, "new failure", "added here, fail to build"),
        (Absent, Blocked) => (
            3,
            "new blocked package",
            "added here, blocked by a failed dependency",
        ),
        (Unknown, Failed) => (4, "failure", "fail here; base status unknown"),
        (Unknown, Blocked) => (5, "blocked package", "blocked here; base status unknown"),
        (Skipped, Failed) => (6, "failure", "skipped on the base, fail here"),
        (Skipped, Blocked) => (
            7,
            "blocked package",
            "skipped on the base, a dependency fails here",
        ),
        (Failed, Failed) => (8, "pre-existing failure", "fail on the base and here"),
        (Failed, Blocked) => (9, "pre-existing failure", "fail on the base, blocked here"),
        (Blocked, Failed) => (10, "pre-existing failure", "blocked on the base, fail here"),
        (Blocked, Blocked) => (
            11,
            "pre-existing blocked package",
            "blocked on the base and here",
        ),
        (Built, Skipped) => (
            12,
            "newly skipped package",
            "build on the base, skipped here (not attempted)",
        ),
        (Failed, Skipped) => (
            13,
            "newly skipped package",
            "fail on the base, skipped here (not attempted)",
        ),
        (Blocked, Skipped) => (
            14,
            "newly skipped package",
            "blocked on the base, skipped here (not attempted)",
        ),
        (Absent, Skipped) => (
            15,
            "new skipped package",
            "added here, already skipped (not attempted)",
        ),
        (Unknown, Skipped) => (
            16,
            "skipped package",
            "skipped here (not attempted); base status unknown",
        ),
        (Skipped, Skipped) => (
            17,
            "pre-existing skipped package",
            "skipped on the base and here (not attempted)",
        ),
        (Built, Absent) => (18, "dropped package", "build on the base, gone here"),
        (Failed, Absent) => (19, "removed package", "failed on the base, gone here"),
        (Blocked, Absent) => (20, "removed package", "blocked on the base, gone here"),
        (Skipped, Absent) => (
            21,
            "removed skipped package",
            "skipped on the base, gone here",
        ),
        (Failed, Built) => (22, "fixed package", "fail on the base, build here"),
        (Blocked, Built) => (23, "fixed package", "blocked on the base, build here"),
        (Skipped, Built) => (
            24,
            "newly enabled package",
            "skipped on the base, build here",
        ),
        (Absent, Built) => (25, "new package", "new here, build"),
        (Unknown, Built) => (26, "built package", "build here; base status unknown"),
        (Built, Built) => (27, "unchanged package", "build on the base and here"),
        // A head-side Unknown is only reachable with --no-build (§8): the drv
        // exists but nothing has been built or probed yet.
        (Built, Unknown) => (28, "unbuilt package", "build on the base; no fact here yet"),
        (Failed, Unknown) => (29, "unbuilt package", "fail on the base; no fact here yet"),
        (Blocked, Unknown) => (
            30,
            "unbuilt package",
            "blocked on the base; no fact here yet",
        ),
        (Skipped, Unknown) => (
            31,
            "unbuilt package",
            "skipped on the base; no fact here yet",
        ),
        (Absent, Unknown) => (32, "new unbuilt package", "added here; no fact yet"),
        (Unknown, Unknown) => (33, "unbuilt package", "no facts on either side yet"),
        (Unknown, Absent) => (34, "removed package", "gone here; base status unknown"),
        // Not producible by the diff (a changed row has a drv on at least one
        // side), but the renderer shouldn't panic if it ever appears.
        (Absent, Absent) => (35, "package", "absent on both sides"),
    }
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

    let (_, noun, phrase) = cell(base, head);
    let plural = if groups == 1 { "" } else { "s" };
    // Note the raw attr count too, but only when grouping actually collapsed rows.
    let note = if attrs_total != groups {
        format!(" ({attrs_total} attrs)")
    } else {
        String::new()
    };

    // Sections carry their own separator *before* them (a leading blank line),
    // so the gaps fall between sections and none trails the last one.
    let mut s = format!(
        "\n<details><summary>{} → {} · <b>{groups} {noun}{plural}</b>{note} — {phrase}</summary>\n\n",
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

/// Render the per-system entries to Markdown, grouped into `before → after`
/// sections ordered worst-delta-first.
pub fn render(base: &str, head: &str, per_system: &[(String, Vec<Entry>)]) -> String {
    // Bare commit hashes (no code span) so GitHub auto-links them as short SHAs.
    let mut out = format!("## `npd` report: {base} → {head}\n");
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
        // The state pair tie-breaks equal priorities (several pairs share the
        // generic last cell), keeping the output deterministic.
        keys.sort_by_key(|&(b, h)| (cell(b, h).0, b, h));
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
    fn cell_priorities_are_distinct() {
        // Every (base, head) pair has its own section slot; a duplicate
        // priority would silently merge two sections' ordering.
        use State::{Absent, Blocked, Built, Failed, Skipped, Unknown};
        const ALL: [State; 6] = [Built, Failed, Blocked, Skipped, Absent, Unknown];
        let mut seen = std::collections::HashSet::new();
        for b in ALL {
            for h in ALL {
                let (priority, noun, phrase) = cell(b, h);
                assert!(seen.insert(priority), "duplicate priority {priority}");
                assert!(!noun.is_empty() && !phrase.is_empty());
            }
        }
        assert_eq!(seen.len(), 36);
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
        let out = render("base", "head", &[("aarch64-linux".into(), entries)]);

        // Composable tokens and the transitive distinction.
        assert!(out.contains("✅ → ❌ · <b>1 regression</b>"), "{out}");
        assert!(out.contains("✅ → 🚫 · <b>2 blocked packages</b>"), "{out}");
        assert!(
            out.contains("✅ → ⏩ · <b>1 newly skipped package</b>"),
            "{out}"
        );
        // Grouping: shared drv collapses to one equals-joined line, shortest first.
        assert!(out.contains("- `foo` = `z.foo`"), "{out}");
        assert!(
            out.contains("✅ → ✅ · <b>1 unchanged package</b> (2 attrs)"),
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
