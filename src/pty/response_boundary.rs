//! Response boundary detector.
//!
//! Watches PTY byte streams for the runner's "idle" prompt marker and,
//! when found, packages everything since the last `on_prompt_submitted`
//! call into a [`StoredTurn`] and pushes it through a [`TurnSink`].
//!
//! See `docs/designs/session-management.md` §4 and the Phase 3 plan in
//! `docs/plans/session-management-phase-1-3.md`.
//!
//! ## Scope
//!
//! This is *not* a terminal emulator. It holds a per-session byte
//! buffer, does a cheap-and-dirty ANSI strip pass on boundary check,
//! and regex-matches the configured idle marker. That's enough for the
//! common case — the runner streams tokens then returns to its input
//! prompt — and it avoids re-running `vt100` twice (once in the TUI,
//! once here).
//!
//! The real Claude Code idle-prompt pattern is not yet pinned; Phase 3
//! Task 8's real-Claude integration test will supply it. Until then,
//! `for_claude_code()` ships a placeholder and tests use `new` with a
//! synthetic `## DONE` marker.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;

use regex::Regex;

use crate::session::{StoredTurn, TurnId, TurnSink};

static RE_CLAUDE_IDLE_PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"__CCOM_PLACEHOLDER_CLAUDE_IDLE__")
        .expect("RE_CLAUDE_IDLE_PLACEHOLDER is a valid regex")
});

/// Per-session response boundary detector.
///
/// Holds one [`PerSessionState`] per `session_id` it's seen. Callers
/// drive it with three methods:
///
/// - [`on_prompt_submitted`](Self::on_prompt_submitted) — begin a turn
/// - [`on_pty_data`](Self::on_pty_data) — feed bytes
/// - [`check_for_boundary`](Self::check_for_boundary) — maybe push
pub struct ResponseBoundaryDetector {
    /// Pattern that marks the runner returning to its idle input
    /// prompt — i.e. the response is complete. For Claude Code this
    /// will eventually be configured to the actual prompt regex
    /// (TODO: pinned by the Phase 3 Task 8 real-Claude integration
    /// test); for now, callers configure their own pattern.
    idle_marker: Regex,
    states: HashMap<usize, PerSessionState>,
}

#[derive(Default)]
struct PerSessionState {
    active_turn: Option<TurnId>,
    started_at: Option<Instant>,
    body_bytes: Vec<u8>,
}

impl ResponseBoundaryDetector {
    /// Construct a detector with a custom idle-marker regex. Tests
    /// inject a synthetic marker like `r"## DONE"`; production (after
    /// Phase 3 Task 8) will use Claude Code's actual idle prompt
    /// pattern.
    ///
    /// **⚠️ The idle marker is matched against the ANSI-stripped,
    /// UTF-8-lossy body text** — not the raw PTY bytes. This means:
    ///
    /// - A marker pattern that contains an escape sequence
    ///   (e.g. `\x1b[0m> ` for "reset color, then prompt") will
    ///   **never** match, because [`ansi_strip`] removes the escape
    ///   before the regex sees it.
    /// - Markers should be plain text that appears verbatim in the
    ///   stripped output. Cursor positioning, color resets, and
    ///   other terminal control sequences are gone by the time the
    ///   regex runs.
    /// - If your marker needs to express "after a reset, an input
    ///   prompt appears," express the *visible* form: e.g.
    ///   `r"^> $"` (start-of-line then `> `).
    ///
    /// PR #9 review item C2 added this warning. The
    /// `for_claude_code()` placeholder is plain text so it doesn't
    /// hit the gotcha — but the future empirical pinning step needs
    /// to be aware.
    pub fn new(idle_marker: Regex) -> Self {
        Self {
            idle_marker,
            states: HashMap::new(),
        }
    }

    /// Convenience constructor for Claude Code. **Currently a stub:**
    /// returns a detector with a placeholder pattern. The real pattern
    /// will be filled in by the Phase 3 Task 8 real-Claude integration
    /// test once we have empirical data on Claude Code's idle prompt
    /// shape. Until then, this constructor is annotated and tests use
    /// `new` with a synthetic marker.
    ///
    /// **⚠️ When you pin the real pattern**, remember that
    /// [`ResponseBoundaryDetector::new`] matches the regex against
    /// **ANSI-stripped, UTF-8-lossy body text** — not raw PTY bytes.
    /// Capture Claude Code's actual idle prompt by running it once,
    /// piping the output through [`ansi_strip`] (or just `cat -v`),
    /// and writing your regex against *that* form. PR #9 review C2.
    pub fn for_claude_code() -> Self {
        // PLACEHOLDER: a pattern that is extremely unlikely to appear
        // in normal output. Phase 3 Task 8 will replace this with the
        // real empirically-pinned Claude Code idle prompt shape.
        Self::new(RE_CLAUDE_IDLE_PLACEHOLDER.clone())
    }

    /// Begin tracking a new turn for `session_id`. Resets any
    /// previously-active turn for this session — partial bodies are
    /// discarded. Called from `SessionManager::send_prompt` (wired in
    /// Phase 3 Task 4, not in this task's scope).
    pub fn on_prompt_submitted(&mut self, session_id: usize, turn_id: TurnId) {
        let state = self.states.entry(session_id).or_default();
        state.active_turn = Some(turn_id);
        state.started_at = Some(Instant::now());
        state.body_bytes.clear();
    }

    /// Append bytes to the active turn's body buffer for `session_id`.
    /// No-op if no turn is active. Called from the PTY reader loop
    /// (wired in Phase 3 Task 4, not in this task's scope).
    pub fn on_pty_data(&mut self, session_id: usize, data: &[u8]) {
        let Some(state) = self.states.get_mut(&session_id) else {
            return;
        };
        if state.active_turn.is_none() {
            return;
        }
        state.body_bytes.extend_from_slice(data);
    }

    /// Drop all per-session state for `session_id`. Called from
    /// `SessionManager::kill` and from the `reap_exited` transition
    /// path so the detector's internal `HashMap` doesn't grow
    /// unboundedly across the lifetime of a long-running TUI.
    /// PR #9 review item C1.
    ///
    /// No-op if the detector has never seen this session.
    pub fn forget_session(&mut self, session_id: usize) {
        self.states.remove(&session_id);
    }

    /// Test seam: report whether the detector still has any state
    /// for `session_id`. Used by the regression test that pins the
    /// `forget_session` cleanup contract.
    #[cfg(test)]
    pub(crate) fn knows_session(&self, session_id: usize) -> bool {
        self.states.contains_key(&session_id)
    }

    /// Complete the active turn for `session_id` using an
    /// externally-supplied body. Used by hook-based boundary
    /// detection (Phase 3.5): the Stop hook fires with
    /// `last_assistant_message` as the body, eliminating the need
    /// for regex-based visual inference.
    ///
    /// If there is no active turn for this session, returns `false`
    /// and the body is silently dropped. (This happens when the user
    /// types into the session directly — no `send_prompt`, no
    /// `TurnId`, nothing to complete.)
    pub fn complete_active_turn_with_body<S: TurnSink>(
        &mut self,
        session_id: usize,
        body: String,
        sink: &mut S,
    ) -> bool {
        let Some(state) = self.states.get_mut(&session_id) else {
            return false;
        };
        let Some(turn_id) = state.active_turn else {
            return false;
        };

        let started_at = state.started_at.unwrap_or_else(Instant::now);
        let stored = StoredTurn {
            turn_id,
            started_at,
            completed_at: Some(Instant::now()),
            body,
        };

        state.active_turn = None;
        state.started_at = None;
        state.body_bytes.clear();

        sink.push_turn(stored);
        true
    }

    /// Check whether the active turn for `session_id` has ended
    /// (idle marker found). If so, push the completed `StoredTurn`
    /// to `sink` and reset session state.
    pub fn check_for_boundary<S: TurnSink>(&mut self, session_id: usize, sink: &mut S) {
        let Some(state) = self.states.get_mut(&session_id) else {
            return;
        };
        let Some(turn_id) = state.active_turn else {
            return;
        };

        let raw = String::from_utf8_lossy(&state.body_bytes);
        let stripped = ansi_strip(&raw);

        if !self.idle_marker.is_match(&stripped) {
            return;
        }

        let started_at = state.started_at.unwrap_or_else(Instant::now);
        let stored = StoredTurn {
            turn_id,
            started_at,
            completed_at: Some(Instant::now()),
            body: stripped,
        };

        state.active_turn = None;
        state.started_at = None;
        state.body_bytes.clear();

        sink.push_turn(stored);
    }
}

/// Strip CSI sequences (`ESC[…<final>`) and OSC sequences
/// (`ESC]…BEL` / `ESC]…ESC\`) from `input`, returning a plain-text
/// `String`. Other ESC sequences (charset selection, single-char
/// escapes) are stripped best-effort: ESC followed by a non-`[`,
/// non-`]` byte consumes both bytes. A trailing orphan ESC at
/// end-of-input is dropped.
///
/// Not a full terminal emulator — just enough to make a response body
/// human-readable for bus subscribers. The real TUI still parses
/// through `vt100` for display purposes.
fn ansi_strip(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != 0x1b {
            out.push(b);
            i += 1;
            continue;
        }
        // ESC seen. Peek at next byte.
        let Some(&next) = bytes.get(i + 1) else {
            // Orphan ESC at end-of-input — drop it.
            break;
        };
        match next {
            b'[' => {
                // CSI: ESC [ params... final
                // Final byte is in 0x40..=0x7E. Params/intermediates
                // are 0x20..=0x3F — skip until we hit a final byte.
                i += 2;
                while i < bytes.len() {
                    let c = bytes[i];
                    i += 1;
                    if (0x40..=0x7E).contains(&c) {
                        break;
                    }
                }
            }
            b']' => {
                // OSC: ESC ] ... terminator (BEL or ESC \).
                i += 2;
                while i < bytes.len() {
                    let c = bytes[i];
                    if c == 0x07 {
                        // BEL terminator.
                        i += 1;
                        break;
                    }
                    if c == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        // ESC \ terminator (ST).
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                // Other single-char ESC sequence (charset select,
                // etc.). Drop ESC + next byte.
                i += 2;
            }
        }
    }
    // `out` is derived from a valid UTF-8 `&str`; ANSI control bytes
    // are all ASCII so removing them leaves valid UTF-8 intact.
    String::from_utf8(out).expect("ansi_strip preserves utf8")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- helpers ----------------

    fn load_fixture(name: &str) -> anyhow::Result<Vec<u8>> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pty")
            .join(name);
        Ok(std::fs::read(&path)?)
    }

    fn synthetic_detector() -> ResponseBoundaryDetector {
        ResponseBoundaryDetector::new(Regex::new(r"## DONE").unwrap())
    }

    struct RecordingSink {
        turns: Vec<StoredTurn>,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self { turns: Vec::new() }
        }
    }

    impl TurnSink for RecordingSink {
        fn push_turn(&mut self, turn: StoredTurn) {
            self.turns.push(turn);
        }
    }

    // ---------------- ansi_strip ----------------

    #[test]
    fn ansi_strip_passthroughs_plain_text() {
        assert_eq!(ansi_strip("hello world"), "hello world");
    }

    #[test]
    fn ansi_strip_removes_csi_color_sequences() {
        assert_eq!(ansi_strip("\x1b[32mhello\x1b[0m"), "hello");
    }

    #[test]
    fn ansi_strip_removes_csi_cursor_sequences() {
        assert_eq!(ansi_strip("\x1b[2;1Habc\x1b[K"), "abc");
    }

    #[test]
    fn ansi_strip_removes_osc_sequences() {
        assert_eq!(ansi_strip("\x1b]0;title\x07rest"), "rest");
    }

    #[test]
    fn ansi_strip_handles_orphan_escape() {
        // Documented choice: an orphan ESC at end-of-input is dropped.
        assert_eq!(ansi_strip("\x1b"), "");
    }

    // ---------------- detector: basic flow ----------------

    #[test]
    fn detector_drops_bytes_when_no_active_turn() {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        // Bytes before any prompt is submitted must be dropped.
        det.on_pty_data(1, b"stray");

        det.on_prompt_submitted(1, TurnId::new(0));
        det.check_for_boundary(1, &mut sink);

        assert!(sink.turns.is_empty());
    }

    #[test]
    fn detector_pushes_completed_turn_on_idle_marker() -> anyhow::Result<()> {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        let fixture = load_fixture("short_response.bin")?;
        det.on_prompt_submitted(1, TurnId::new(0));
        det.on_pty_data(1, &fixture);
        det.check_for_boundary(1, &mut sink);

        assert_eq!(sink.turns.len(), 1);
        assert_eq!(sink.turns[0].turn_id, TurnId::new(0));
        assert!(
            sink.turns[0].body.contains("Hello! How can I help you?"),
            "body was {:?}",
            sink.turns[0].body
        );
        assert!(sink.turns[0].completed_at.is_some());
        Ok(())
    }

    #[test]
    fn detector_strips_ansi_from_body() -> anyhow::Result<()> {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        let fixture = load_fixture("with_ansi.bin")?;
        det.on_prompt_submitted(1, TurnId::new(0));
        det.on_pty_data(1, &fixture);
        det.check_for_boundary(1, &mut sink);

        assert_eq!(sink.turns.len(), 1);
        let body = &sink.turns[0].body;
        assert!(
            !body.contains('\x1b'),
            "expected ANSI-stripped body, got {:?}",
            body
        );
        assert!(body.contains("Hello! How can I help you?"));
        Ok(())
    }

    #[test]
    fn detector_handles_chunked_arrival() -> anyhow::Result<()> {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        let fixture = load_fixture("multi_chunk.bin")?;
        det.on_prompt_submitted(1, TurnId::new(0));

        // Slice into ~3 chunks at arbitrary byte boundaries.
        let n = fixture.len();
        let a = n / 3;
        let b = (2 * n) / 3;
        let chunks: [&[u8]; 3] = [&fixture[..a], &fixture[a..b], &fixture[b..]];

        for chunk in chunks {
            det.on_pty_data(1, chunk);
            det.check_for_boundary(1, &mut sink);
        }

        assert_eq!(sink.turns.len(), 1);
        assert!(sink.turns[0].body.contains("Here's the answer: 42."));
        Ok(())
    }

    #[test]
    fn detector_does_not_push_without_idle_marker() -> anyhow::Result<()> {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        let fixture = load_fixture("no_marker.bin")?;
        det.on_prompt_submitted(1, TurnId::new(0));
        det.on_pty_data(1, &fixture);
        det.check_for_boundary(1, &mut sink);
        assert!(sink.turns.is_empty());

        // Repeated polling must not synthesize a turn out of thin air.
        for _ in 0..5 {
            det.check_for_boundary(1, &mut sink);
        }
        assert!(sink.turns.is_empty());
        Ok(())
    }

    #[test]
    fn detector_resets_after_pushing() -> anyhow::Result<()> {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        let fixture = load_fixture("short_response.bin")?;
        det.on_prompt_submitted(1, TurnId::new(0));
        det.on_pty_data(1, &fixture);
        det.check_for_boundary(1, &mut sink);
        assert_eq!(sink.turns.len(), 1);

        // Any bytes after the push — without a new prompt_submitted —
        // must be dropped.
        det.on_pty_data(1, b"more bytes ## DONE");
        det.check_for_boundary(1, &mut sink);
        assert_eq!(sink.turns.len(), 1);
        Ok(())
    }

    // ---------------- detector: multi-turn ----------------

    #[test]
    fn detector_handles_two_consecutive_turns() -> anyhow::Result<()> {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        let fixture = load_fixture("two_turns.bin")?;
        // Find the end of the first "## DONE\n" block.
        let marker = b"## DONE\n";
        let first_end = {
            let mut idx = None;
            for i in 0..=fixture.len().saturating_sub(marker.len()) {
                if &fixture[i..i + marker.len()] == marker {
                    idx = Some(i + marker.len());
                    break;
                }
            }
            idx.expect("first marker present in two_turns.bin")
        };

        let (first, second) = fixture.split_at(first_end);

        det.on_prompt_submitted(1, TurnId::new(0));
        det.on_pty_data(1, first);
        det.check_for_boundary(1, &mut sink);

        det.on_prompt_submitted(1, TurnId::new(1));
        det.on_pty_data(1, second);
        det.check_for_boundary(1, &mut sink);

        assert_eq!(sink.turns.len(), 2);
        assert_eq!(sink.turns[0].turn_id, TurnId::new(0));
        assert_eq!(sink.turns[1].turn_id, TurnId::new(1));
        assert!(sink.turns[0].body.contains("First answer here."));
        assert!(sink.turns[1].body.contains("Second answer here."));
        Ok(())
    }

    #[test]
    fn detector_isolates_per_session_state() -> anyhow::Result<()> {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        det.on_prompt_submitted(1, TurnId::new(0));
        det.on_prompt_submitted(2, TurnId::new(0));

        // Only session 1 receives bytes.
        let fixture = load_fixture("short_response.bin")?;
        det.on_pty_data(1, &fixture);

        det.check_for_boundary(1, &mut sink);
        det.check_for_boundary(2, &mut sink);

        assert_eq!(sink.turns.len(), 1);
        // The one pushed turn must be session 1's (id 0).
        assert_eq!(sink.turns[0].turn_id, TurnId::new(0));
        assert!(sink.turns[0].body.contains("Hello!"));
        Ok(())
    }

    #[test]
    fn detector_re_submitted_prompt_discards_partial_body() {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        det.on_prompt_submitted(1, TurnId::new(0));
        det.on_pty_data(1, b"partial response without marker");

        // User submits a new prompt before the previous response
        // finished — the partial body must be discarded.
        det.on_prompt_submitted(1, TurnId::new(1));
        det.on_pty_data(1, b"fresh body\n## DONE\n");
        det.check_for_boundary(1, &mut sink);

        assert_eq!(sink.turns.len(), 1);
        assert_eq!(sink.turns[0].turn_id, TurnId::new(1));
        let body = &sink.turns[0].body;
        assert!(body.contains("fresh body"), "body was {:?}", body);
        assert!(
            !body.contains("partial response"),
            "body must not leak prior partial: {:?}",
            body
        );
    }

    // ---------------- hook-based completion ----------------

    #[test]
    fn complete_active_turn_with_body_pushes_stored_turn() {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        det.on_prompt_submitted(1, TurnId::new(5));
        let completed =
            det.complete_active_turn_with_body(1, "hook-supplied body".to_string(), &mut sink);

        assert!(completed);
        assert_eq!(sink.turns.len(), 1);
        assert_eq!(sink.turns[0].turn_id, TurnId::new(5));
        assert_eq!(sink.turns[0].body, "hook-supplied body");
        assert!(sink.turns[0].completed_at.is_some());
    }

    #[test]
    fn complete_active_turn_with_body_returns_false_without_active_turn() {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        // No prompt submitted → no active turn.
        let completed = det.complete_active_turn_with_body(1, "orphan body".to_string(), &mut sink);

        assert!(!completed);
        assert!(sink.turns.is_empty());
    }

    #[test]
    fn complete_active_turn_with_body_clears_active_turn() {
        let mut det = synthetic_detector();
        let mut sink = RecordingSink::new();

        det.on_prompt_submitted(1, TurnId::new(0));
        det.complete_active_turn_with_body(1, "first".to_string(), &mut sink);

        // Second call without a new prompt should be a no-op.
        let completed = det.complete_active_turn_with_body(1, "stray".to_string(), &mut sink);
        assert!(!completed);
        assert_eq!(sink.turns.len(), 1);
    }

    // ---------------- constructor ----------------

    #[test]
    fn for_claude_code_constructs_without_panic() {
        // Placeholder marker until Phase 3 Task 8 nails down the real
        // Claude Code idle prompt shape. This test just pins that the
        // constructor exists and doesn't crash.
        let _det = ResponseBoundaryDetector::for_claude_code();
    }
}
