//! A small inline multi-line live display — the eval progress readout
//! (`crate::eval`). It redraws a block of lines in place on stderr, each line
//! **truncated to the current terminal width** so it always occupies exactly
//! one row. That one invariant is the whole point: moving the cursor up `n` rows
//! then lands on the block's top even after the window is resized, whereas
//! indicatif pads every line out to the full width — which reflows into garbage
//! the moment the width changes (its cursor math is fixed at the *previous*
//! width). Truncated content leaves nothing to reflow.
//!
//! **Flicker-free, especially over a laggy SSH link.** A frame is built as one
//! string and written once (one packet, not one per line), and:
//! - content is *overwritten in place* then the tail cleared (`content` + `\x1b[K`),
//!   never blanked first (`\x1b[2K` then write) — so there's no blank flash while
//!   the new bytes are in flight;
//! - lines unchanged since the last frame are skipped (the cursor just steps
//!   over them), so a steady line isn't rewritten 10×/s;
//! - the whole frame is wrapped in the *synchronized output* private mode
//!   (`\x1b[?2026h`…`l`), so terminals that support it (iTerm2, kitty, WezTerm,
//!   tmux ≥3.4) render it atomically — no tearing — and others ignore it.
//!
//! Render-only: no raw mode, no alternate screen, cursor left visible. So a ^C
//! mid-run just leaves the last (short, unpadded) block on screen, which reflows
//! like ordinary command output rather than the old full-width mess — no signal
//! handler required to keep resize sane.
//!
//! ratatui's inline viewport was the other renderer considered: it re-queries the
//! width and re-lays out a diffed frame each draw, so resize is free — but it must
//! hide the cursor and restore it on *every* exit path including ^C (a missed
//! teardown leaks a hidden cursor into the shell), and it anchors itself with a DSR
//! cursor-position query the terminal must answer, so it errors under a pipe or a
//! non-interactive pty. This relative-move renderer needs neither.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use console::{Term, style, truncate_str};

/// Braille spinner frames (indicatif's default set).
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The cyan spinner glyph for tick `n` — the leading char of a timer line.
/// Callers advance `n` once per redraw to animate it.
pub fn spinner(n: usize) -> String {
    style(SPINNER[n % SPINNER.len()]).cyan().to_string()
}

/// A block of lines redrawn in place. `drawn` is the number of rows the last
/// frame occupied — equal to the line count, since every line is one row.
pub struct Live {
    term: Term,
    drawn: usize,
    /// Lines shown last frame, and the width they were truncated at — so a frame
    /// only rewrites the lines that changed (and forces a full redraw on resize).
    prev: Vec<String>,
    prev_width: usize,
}

impl Live {
    pub fn new() -> Self {
        Self {
            term: Term::stderr(),
            drawn: 0,
            prev: Vec::new(),
            prev_width: 0,
        }
    }

    fn width(&self) -> usize {
        self.term.size_checked().map_or(80, |(_, w)| w as usize)
    }

    /// Redraw the block in place. A no-op on a non-terminal stderr (piped / CI):
    /// there is no cursor to move, and the caller's final summary still prints.
    pub fn draw(&mut self, lines: &[String]) {
        if !self.term.is_term() {
            return;
        }
        let w = self.width();
        // A resize changes every line's truncation, so redraw all lines then.
        let full = w != self.prev_width || lines.len() != self.prev.len();

        let mut buf = String::from("\x1b[?2026h"); // begin synchronized update
        if self.drawn > 0 {
            let _ = write!(buf, "\x1b[{}A", self.drawn); // up to the block's top row
        }
        buf.push('\r');
        for (i, line) in lines.iter().enumerate() {
            if full || self.prev.get(i).map(String::as_str) != Some(line.as_str()) {
                // Overwrite in place, then clear only the tail — no blank flash.
                buf.push_str(&truncate_str(line, w, ""));
                buf.push_str("\x1b[K");
            }
            buf.push_str("\r\n"); // step to column 0 of the next row
        }
        // Fewer lines than last frame? Erase the now-orphaned rows below.
        if lines.len() < self.drawn {
            buf.push_str("\x1b[J");
        }
        buf.push_str("\x1b[?2026l"); // end synchronized update
        let _ = self.term.write_str(&buf);
        let _ = self.term.flush();

        self.drawn = lines.len();
        self.prev = lines.to_vec();
        self.prev_width = w;
    }

    /// Emit `msg` as permanent output *above* the live block (a one-off note,
    /// e.g. a requeued shard). The block is erased and reappears on the next
    /// [`Live::draw`], below the now-permanent message.
    pub fn print_above(&mut self, msg: &str) {
        if !self.term.is_term() {
            eprintln!("{msg}");
            return;
        }
        let w = self.width();
        let mut buf = String::from("\x1b[?2026h");
        if self.drawn > 0 {
            let _ = write!(buf, "\x1b[{}A", self.drawn);
        }
        buf.push_str("\r\x1b[J"); // to the block's top, erase it and everything below
        for l in msg.lines() {
            buf.push_str(&truncate_str(l, w, ""));
            buf.push_str("\x1b[K\r\n");
        }
        buf.push_str("\x1b[?2026l");
        let _ = self.term.write_str(&buf);
        let _ = self.term.flush();
        self.drawn = 0;
        self.prev.clear(); // next draw redraws the block in full
    }

    /// Erase the block, leaving the cursor at its top. The caller then prints a
    /// clean, unpadded final summary as ordinary output.
    pub fn clear(&mut self) {
        if self.term.is_term() && self.drawn > 0 {
            let _ = self
                .term
                .write_str(&format!("\x1b[{}A\r\x1b[J", self.drawn));
            let _ = self.term.flush();
        }
        self.drawn = 0;
        self.prev.clear();
    }
}

/// A handle into a running [`with_live`] block, handed to the worker body so it
/// can emit permanent output *above* the animated region (a one-off note like a
/// requeued shard). [`Copy`] so the body can share it across its own workers.
#[derive(Clone, Copy)]
pub struct LiveHandle<'a> {
    display: &'a Mutex<Live>,
}

impl LiveHandle<'_> {
    /// Print `msg` as permanent output above the live block; the block redraws
    /// below it on the next frame. Thread-safe — the workers and the refresher
    /// share the one `Live` behind a mutex.
    pub fn note(&self, msg: &str) {
        self.display.lock().unwrap().print_above(msg);
    }
}

/// Run `body` while a refresher thread animates a live progress block on stderr.
///
/// This is npd's single progress-display primitive: every phase that shows a
/// live readout — the shard scheduler ([`crate::eval::run_shards`], which backs
/// eval, `--tests`, enumeration, and instantiation) and the cache probe
/// ([`crate::build`]) — drives it through here, so they all animate identically
/// (a steady 100 ms redraw that keeps the spinner + timer moving even while the
/// work itself is silent) and tear down identically. `frame(tick)` returns the
/// block's lines for tick `tick` — the caller composes its own spinner/timer via
/// [`spinner`]/[`human_elapsed`] — and is only ever called from the refresher,
/// reading whatever atomics `body`'s workers bump (so those need no locking).
/// When `body` returns, the block is erased (the caller then prints any frozen
/// summary as ordinary output) and `body`'s value is returned; `body` gets a
/// [`LiveHandle`] for notes above the block.
pub fn with_live<R>(
    frame: impl Fn(usize) -> Vec<String> + Sync,
    body: impl FnOnce(LiveHandle<'_>) -> R,
) -> R {
    let display = Mutex::new(Live::new());
    let done = AtomicBool::new(false);
    let mut out = None;
    thread::scope(|s| {
        let (display, done, frame) = (&display, &done, &frame);
        s.spawn(move || {
            let mut tick = 0usize;
            while !done.load(Ordering::Relaxed) {
                display.lock().unwrap().draw(&frame(tick));
                thread::sleep(Duration::from_millis(100));
                tick += 1;
            }
        });
        out = Some(body(LiveHandle { display }));
        done.store(true, Ordering::Relaxed);
    });
    display.lock().unwrap().clear();
    out.unwrap()
}

/// Elapsed time as a plain `m:ss` clock, gaining an `h:` field once past an
/// hour, right-padded with spaces to a fixed width so the text after the timer
/// doesn't shift as it grows.
pub fn human_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    // `h`/`m`/`s` fields, dropping empty leading ones: `0s`, `51s`, `1m29s`,
    // `1h00m00s`. Lower fields are zero-padded once a higher one is present so
    // they don't jump width. The widest form is `9h59m59s` (8 chars, up to ~10h);
    // right-pad the rest so the text after the clock doesn't shift as it grows.
    let clock = if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    };
    format!("{clock:>8}")
}

// --- the progress tree (DESIGN §6, §9) ---------------------------------------
//
// One persistent, append-only tree spanning eval → probe: every piece of
// network or nontrivial work becomes a node the moment npd learns it needs it,
// nothing is ever removed, and cached/no-op work never appears at all. Phases
// (`enumerate`, `evaluate`, `tests`, `instantiate`, `probe`, and the network
// `fetch`/`download`) are top-level nodes; under them a system level (elided for
// a single-system run) and the per-side commit `display`s. State is one of three
// nom colors — blue waiting, yellow running, green done — carried by the label;
// counts are plain, the `/ total` denominator dim, nothing bold. See the
// rendering spec in `scratch/tree_demo.py`.

// Dull ANSI, matching nom (`lib/NOM/Print.hs`) and the demo.
const BLUE: &str = "\x1b[34m";
const YELLOW: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const DIM: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

/// Two spaces per tree level.
const INDENT: &str = "  ";
/// Fixed width of each number column (a count, and its total). Six digits covers
/// the largest count — the ~119k attr eval — with headroom, and being fixed
/// means a count gaining a digit never shifts the column.
const NUM_W: usize = 6;

/// The three node states — the blue/yellow/green of nom.
const WAIT: u8 = 0;
const RUN: u8 = 1;
const DONE: u8 = 2;

/// A node in the progress [`Tree`]. Workers bump its atomics lock-free while the
/// refresher reads them, so updates need no locking. Two columns follow the
/// label: a middle plain **count** (the streamed item/drv count), shown for any
/// `counter` node once active; and a rightmost dim column shown only *while
/// running* — either ` / total` (a known item total, e.g. `instantiate`/`probe`)
/// or an `NN%` shard-progress readout (a `percent` node, e.g. `evaluate`, whose
/// true drv total is unknowable). A count-less node (a phase, a system, a network
/// ref, `enumerate`) has neither — just a state color.
pub struct Node {
    label: String,
    depth: usize,
    counter: bool,
    percent: bool,
    state: AtomicU8,
    /// Items/drvs streamed so far — the middle plain count.
    count: AtomicI64,
    /// Item total for the ` / total` column, or `-1` when unknown / not a total
    /// node. (A `percent` node leaves this `-1`; its `%` comes from the shards.)
    total: AtomicI64,
    /// Shards done / currently-running / total, for a `percent` node's `NN%`
    /// readout. Counting a running shard as half-done makes the percentage climb
    /// smoothly rather than only stepping when a whole shard lands.
    shards_done: AtomicI64,
    shards_running: AtomicI64,
    shards_total: AtomicI64,
}

impl Node {
    fn new(label: String, depth: usize, counter: bool, percent: bool, total: i64) -> Self {
        Self {
            label,
            depth,
            counter,
            percent,
            state: AtomicU8::new(WAIT),
            count: AtomicI64::new(0),
            total: AtomicI64::new(total),
            shards_done: AtomicI64::new(0),
            shards_running: AtomicI64::new(0),
            shards_total: AtomicI64::new(0),
        }
    }

    /// Move `WAIT` → `RUN`; never regress a node that has already finished (so
    /// concurrent shards of one group race harmlessly).
    pub fn set_running(&self) {
        let _ = self
            .state
            .compare_exchange(WAIT, RUN, Ordering::Relaxed, Ordering::Relaxed);
    }

    pub fn set_done(&self) {
        self.state.store(DONE, Ordering::Relaxed);
    }

    /// Add `n` to the running count (drives the live middle number).
    pub fn add_count(&self, n: i64) {
        self.count.fetch_add(n, Ordering::Relaxed);
    }

    pub fn set_total(&self, n: i64) {
        self.total.store(n, Ordering::Relaxed);
    }

    /// A streamed item surfaced — drives the middle count for any counter node
    /// (a count-less node has no number).
    pub fn stream(&self, n: i64) {
        if self.counter {
            self.count.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// A shard of this group started running (feeds a `percent` node's `NN%`,
    /// which counts a running shard as half-done for a smoother climb).
    pub fn shard_started(&self) {
        if self.percent {
            self.shards_running.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A shard stopped running (completed or aborted) — the mate to
    /// [`shard_started`], so the running count reflects only in-flight shards.
    pub fn shard_finished(&self) {
        if self.percent {
            self.shards_running.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// A shard of this group *completed* (`done` of `total`). Feeds a `percent`
    /// node's `NN%` column; every other kind ignores it.
    pub fn shard_progress(&self, done: usize, total: usize) {
        if self.percent {
            self.shards_total.store(total as i64, Ordering::Relaxed);
            self.shards_done.store(done as i64, Ordering::Relaxed);
        }
    }

    /// The group's last shard landed with `rows` assembled items: pin the middle
    /// count to the exact total (the streamed tally can drift), then mark done.
    pub fn group_done(&self, rows: i64) {
        if self.counter {
            self.count.store(rows, Ordering::Relaxed);
        }
        self.set_done();
    }
}

/// The one live progress tree, shared (`&Tree`) by every pre-build phase. Nodes
/// are appended under a mutex; their per-node state/counts are lock-free atomics
/// the refresher reads. The number columns start at a width fixed up front (see
/// [`plan_label_width`]) so nothing shifts horizontally as phases appear.
pub struct Tree {
    nodes: Mutex<Vec<Arc<Node>>>,
    start: Instant,
    min_label_w: usize,
    multi: bool,
    /// Whether stderr is a terminal — gates coloring the frozen reprint.
    color: bool,
}

impl Tree {
    pub fn new(min_label_w: usize, multi: bool) -> Self {
        Self {
            nodes: Mutex::new(Vec::new()),
            start: Instant::now(),
            min_label_w,
            multi,
            color: Term::stderr().is_term(),
        }
    }

    /// Whether the run spans more than one system (so phases nest a system level).
    pub fn multi(&self) -> bool {
        self.multi
    }

    /// Append a count-less node (a phase, a system, a network ref, `enumerate`).
    pub fn node(&self, label: impl Into<String>, depth: usize) -> Arc<Node> {
        let n = Arc::new(Node::new(label.into(), depth, false, false, -1));
        self.nodes.lock().unwrap().push(n.clone());
        n
    }

    /// Append a counting leaf; `total` is `-1` when the denominator is unknown.
    pub fn counter(&self, label: impl Into<String>, depth: usize, total: i64) -> Arc<Node> {
        let n = Arc::new(Node::new(label.into(), depth, true, false, total));
        self.nodes.lock().unwrap().push(n.clone());
        n
    }

    /// Append a leaf that shows a dim `NN%` shard-progress readout — for a phase
    /// whose true item total is unknowable ahead of time (`evaluate`).
    pub fn percent(&self, label: impl Into<String>, depth: usize) -> Arc<Node> {
        let n = Arc::new(Node::new(label.into(), depth, true, true, 0));
        self.nodes.lock().unwrap().push(n.clone());
        n
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.lock().unwrap().is_empty()
    }

    /// The live frame for tick `t`: node lines plus a cyan spinner + clock footer.
    pub fn render(&self, t: usize) -> Vec<String> {
        self.lines(Some(t), true)
    }

    /// The frozen reprint (permanent scrollback): the same node lines with a
    /// resting cyan `.` in place of the spinner. Colorized only on a terminal.
    pub fn render_frozen(&self) -> Vec<String> {
        self.lines(None, self.color)
    }

    fn lines(&self, tick: Option<usize>, color: bool) -> Vec<String> {
        let nodes = self.nodes.lock().unwrap();
        if nodes.is_empty() {
            // An empty tree draws nothing at all (a fully-cached run stays quiet).
            return Vec::new();
        }
        // Snapshot the raw per-node fields, then roll parent states up from their
        // descendant leaves.
        let snap: Vec<Row> = nodes
            .iter()
            .map(|n| Row {
                depth: n.depth,
                label: n.label.as_str(),
                counter: n.counter,
                state: n.state.load(Ordering::Relaxed),
                count: n.count.load(Ordering::Relaxed),
                total: n.total.load(Ordering::Relaxed),
                percent: n.percent,
                sdone: n.shards_done.load(Ordering::Relaxed),
                srunning: n.shards_running.load(Ordering::Relaxed),
                stotal: n.shards_total.load(Ordering::Relaxed),
            })
            .collect();
        let eff: Vec<u8> = (0..snap.len()).map(|i| eff_state(&snap, i)).collect();

        // The number columns start past the widest label of ANY node, so a
        // vertical line between the tree and the numbers clips neither.
        let mut left_w = self.min_label_w;
        for r in &snap {
            left_w = left_w.max(INDENT.len() * r.depth + r.label.chars().count());
        }

        let mut out = Vec::with_capacity(snap.len() + 1);
        for (i, r) in snap.iter().enumerate() {
            let (depth, label, counter, count, total, percent, sdone, srunning, stotal) = (
                r.depth, r.label, r.counter, r.count, r.total, r.percent, r.sdone, r.srunning,
                r.stotal,
            );
            let col = state_color(eff[i]);
            let indent = INDENT.repeat(depth);
            // A count populates immediately for any counter node — even while
            // blue (waiting) it reads `0`, rather than appearing only once the
            // node turns yellow. A count-less node (a phase, a system,
            // `enumerate`) never shows a number, just a state color.
            if !counter {
                out.push(if color {
                    format!("{col}{indent}{label}{RESET}")
                } else {
                    format!("{indent}{label}")
                });
                continue;
            }
            let left = format!("{indent}{label}");
            let pad = " ".repeat(left_w.saturating_sub(left.chars().count()));
            let count_s = format!("{count:>NUM_W$}");
            // The rightmost column stays for the node's whole life (waiting →
            // running → done, never dropped): a `percent` node's dim `NN%` (right-
            // aligned in the number column, `%`, no slash), else a dim ` / total`
            // when the item total is known. A running shard counts as half-done —
            // the mean of finished and finished+running shards — so the percentage
            // climbs smoothly instead of only stepping when a whole shard lands.
            let right = if percent {
                let denom = (2 * stotal).max(1);
                let pct = ((2 * sdone + srunning) * 100 / denom).clamp(0, 100);
                let p = format!("{pct:>NUM_W$}");
                if color {
                    format!("{DIM}   {p}%{RESET}")
                } else {
                    format!("   {p}%")
                }
            } else if total >= 0 {
                let t = format!("{total:>NUM_W$}");
                if color {
                    format!("{DIM} / {t}{RESET}")
                } else {
                    format!(" / {t}")
                }
            } else {
                String::new()
            };
            // Only the label carries the state color; the count is plain (like the
            // clock), the ` / total` / percent columns dim.
            if color {
                out.push(format!("{col}{left}{pad}{RESET}  {count_s}{right}"));
            } else {
                out.push(format!("{left}{pad}  {count_s}{right}"));
            }
        }

        let clock = human_elapsed(self.start.elapsed());
        out.push(match tick {
            Some(t) => format!("{} {clock}", spinner(t)),
            None if color => format!("{CYAN}.{RESET} {clock}"),
            None => format!(". {clock}"),
        });
        out
    }
}

/// A rendered snapshot of one [`Node`] — its atomics read once, so the frame is
/// consistent and [`eff_state`] can roll parents up over a plain slice.
struct Row<'a> {
    depth: usize,
    label: &'a str,
    counter: bool,
    state: u8,
    count: i64,
    total: i64,
    percent: bool,
    sdone: i64,
    srunning: i64,
    stotal: i64,
}

/// A node's effective (rolled-up) state: any descendant leaf running → running;
/// all done → done; some done but not all → running; else waiting. A node with
/// no descendants uses its own state.
fn eff_state(snap: &[Row], i: usize) -> u8 {
    let d = snap[i].depth;
    let (mut any_run, mut any_done, mut any_wait, mut any_leaf) = (false, false, false, false);
    let mut j = i + 1;
    while j < snap.len() && snap[j].depth > d {
        let is_leaf = j + 1 >= snap.len() || snap[j + 1].depth <= snap[j].depth;
        if is_leaf {
            any_leaf = true;
            match snap[j].state {
                RUN => any_run = true,
                DONE => any_done = true,
                _ => any_wait = true,
            }
        }
        j += 1;
    }
    if !any_leaf {
        return snap[i].state;
    }
    if any_run {
        RUN
    } else if !any_wait {
        DONE
    } else if any_done {
        RUN
    } else {
        WAIT
    }
}

fn state_color(state: u8) -> &'static str {
    match state {
        RUN => YELLOW,
        DONE => GREEN,
        _ => BLUE,
    }
}

/// The width the tree's number columns start at, computed once up front from
/// every label the run can produce (DESIGN §6): the fixed phase names, the
/// systems, the two `display`s at their nesting depth, and any PR refs or
/// `--patch` compare expr. Passed to [`Tree::new`] so the columns never shift as
/// phases appear (all these labels are known at resolution).
pub fn plan_label_width(systems: &[String], pr: Option<u64>, compare: Option<&str>) -> usize {
    let ind = INDENT.len();
    let mut w = [
        "fetch",
        "download",
        "enumerate",
        "evaluate",
        "tests",
        "instantiate",
        "probe",
    ]
    .iter()
    .map(|p| p.len())
    .max()
    .unwrap();
    // The base/head `display`s are absorbed dynamically: a phase adds all its
    // commit nodes atomically (as WAIT) before any of them shows a number, so the
    // column already clears them by the first frame with a count — nothing shifts,
    // and they need not be known here (they aren't until resolution finishes).
    if systems.len() > 1 {
        for s in systems {
            w = w.max(ind + s.chars().count());
        }
    }
    if let Some(n) = pr {
        w = w.max(ind + format!("refs/pull/{n}/merge").len());
    }
    if let Some(c) = compare {
        w = w.max(ind + c.chars().count());
    }
    w
}

/// npd's one visual separator, on stderr, between each of its phases (the live
/// tree, nom's build, the report): a blank line, a dim rule, a blank line — the
/// spacing does the separating, the rule just marks it. Dimmed only on a
/// terminal, so a redirected stderr gets plain hyphens.
pub fn separator() {
    let rule = "---";
    eprintln!();
    if Term::stderr().is_term() {
        eprintln!("{DIM}{rule}{RESET}");
    } else {
        eprintln!("{rule}");
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_is_a_fixed_width_clock() {
        // h/m/s fields dropping empty leading ones, starting at `0s`, right-padded
        // to a constant width so the text after the timer doesn't shift as it grows.
        assert_eq!(human_elapsed(Duration::from_secs(0)), "      0s");
        assert_eq!(human_elapsed(Duration::from_secs(51)), "     51s");
        assert_eq!(human_elapsed(Duration::from_secs(89)), "   1m29s");
        assert_eq!(human_elapsed(Duration::from_secs(90)), "   1m30s");
        assert_eq!(human_elapsed(Duration::from_secs(3600)), "1h00m00s");
        assert_eq!(human_elapsed(Duration::from_secs(5400)), "1h30m00s");
        // Every rendering up to ~10h is the same width; `9h59m59s` is the widest.
        for s in [0, 51, 599, 3600, 35999] {
            assert_eq!(human_elapsed(Duration::from_secs(s)).len(), 8);
        }
    }

    /// All node lines except the (time-dependent) footer.
    fn node_lines(tree: &Tree) -> Vec<String> {
        let mut lines = tree.render(0);
        lines.pop(); // drop the spinner + clock footer
        lines
    }

    #[test]
    fn renders_states_counts_and_totals() {
        // Single system: phase → commit. Colors live only on the label; the count
        // is plain, the ` / total` dim, nothing bold. A done side collapses to a
        // bare count; a running side shows `count / total`.
        let tree = Tree::new(0, false);
        tree.node("evaluate", 0);
        let base = tree.counter("master", 1, -1);
        let head = tree.counter("HEAD", 1, -1);
        base.set_running();
        base.add_count(114230);
        base.set_done();
        head.set_running();
        head.set_total(114231);
        head.add_count(107347);

        let lines = node_lines(&tree);
        assert_eq!(
            lines,
            vec![
                // rollup: a running child → the phase is yellow.
                "\x1b[33mevaluate\x1b[0m".to_string(),
                // done → green label, bare plain count, aligned in the 8-wide column.
                "\x1b[32m  master\x1b[0m  114230".to_string(),
                // running → yellow label, plain count, dim ` / total`.
                "\x1b[33m  HEAD  \x1b[0m  107347\x1b[90m / 114231\x1b[0m".to_string(),
            ]
        );
    }

    #[test]
    fn percent_node_smooths_and_keeps_pct_when_done() {
        // evaluate: a plain drv count (middle) PLUS a dim shard `NN%` (right). A
        // running shard counts as half-done — mean(finished, finished+running) —
        // so 3 done + 2 running of 10 reads (3 + 5) / 2 = 4 → 40%.
        let tree = Tree::new(0, false);
        tree.node("evaluate", 0);
        let head = tree.percent("HEAD", 1);
        head.set_running();
        head.add_count(142001);
        head.shard_progress(3, 10);
        head.shard_started();
        head.shard_started();
        assert_eq!(
            node_lines(&tree),
            vec![
                "\x1b[33mevaluate\x1b[0m".to_string(),
                "\x1b[33m  HEAD  \x1b[0m  142001\x1b[90m       40%\x1b[0m".to_string(),
            ]
        );
        // Done keeps the percent (now 100%) beside the pinned count — not dropped.
        head.shard_finished();
        head.shard_finished();
        head.shard_progress(10, 10);
        head.group_done(226117);
        assert_eq!(
            node_lines(&tree)[1],
            "\x1b[32m  HEAD  \x1b[0m  226117\x1b[90m      100%\x1b[0m"
        );
    }

    #[test]
    fn waiting_counter_populates_its_number() {
        // A counter populates its number immediately — even while blue (waiting)
        // it reads `0`, rather than blank until it turns yellow.
        let tree = Tree::new(0, false);
        tree.node("tests", 0);
        tree.counter("HEAD", 1, -1); // left in WAIT
        assert_eq!(
            node_lines(&tree),
            vec![
                "\x1b[34mtests\x1b[0m".to_string(),
                "\x1b[34m  HEAD\x1b[0m       0".to_string(),
            ]
        );
    }

    #[test]
    fn rollup_all_done_is_green() {
        let tree = Tree::new(0, false);
        tree.node("enumerate", 0);
        for c in ["master", "HEAD"] {
            let n = tree.counter(c, 1, -1);
            n.set_running();
            n.add_count(100);
            n.set_done();
        }
        assert_eq!(node_lines(&tree)[0], "\x1b[32menumerate\x1b[0m");
    }

    #[test]
    fn empty_tree_draws_nothing() {
        let tree = Tree::new(11, false);
        assert!(tree.is_empty());
        assert!(tree.render(0).is_empty());
        assert!(tree.render_frozen().is_empty());
    }

    #[test]
    fn plan_width_clears_every_label() {
        // Single system: the longest phase name (`instantiate`, 11) is the floor.
        assert_eq!(plan_label_width(&["aarch64-linux".into()], None, None), 11);
        // Multi-system: a system name at depth 1 is widest (2 + 13).
        assert_eq!(
            plan_label_width(&["aarch64-linux".into(), "x86_64-linux".into()], None, None),
            15
        );
        // A PR fetch ref at depth 1 (2 + 19) beats them all.
        assert_eq!(
            plan_label_width(&["aarch64-linux".into()], Some(431), None),
            21
        );
    }
}
