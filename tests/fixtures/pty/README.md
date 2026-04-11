# Synthetic PTY fixtures

Hand-authored byte streams used by `src/pty/response_boundary.rs` tests.

These are **not** captures of real Claude Code output. They use a
synthetic "end of response" protocol — the literal marker `## DONE`
on its own — because Phase 3 Task 3 writes and tests the detector
state machine before the real Claude Code idle-prompt shape is
empirically pinned. That pinning happens in Phase 3 Task 8 (real-Claude
integration test), which will add its own fixtures alongside these.

Tests configure the detector with `Regex::new(r"## DONE").unwrap()`
so the synthetic marker is treated exactly like the eventual real
pattern: the detector ANSI-strips the accumulated body, searches for
the configured regex, and — on match — pushes a `StoredTurn` to the
test's recording sink.

## Fixtures

- **`short_response.bin`** — one complete short response ending in the
  idle marker. Detector should push exactly one turn with body
  containing `"Hello! How can I help you?"`.

- **`with_ansi.bin`** — same body as `short_response.bin` but with
  CSI color/bold sequences sprinkled through it. After ANSI stripping
  the detector's pushed body must equal `short_response.bin`'s body.

- **`multi_chunk.bin`** — a two-line response followed by the marker.
  Tests feed the file to `on_pty_data` in multiple chunks at arbitrary
  byte boundaries to prove the detector accumulates across chunks and
  still fires exactly once.

- **`no_marker.bin`** — an in-progress partial response with no idle
  marker. Detector must never fire for this, regardless of how many
  times `check_for_boundary` is polled.

- **`two_turns.bin`** — two consecutive responses separated by the
  marker. Tests slice this into two halves and drive two
  `on_prompt_submitted` / `on_pty_data` / `check_for_boundary` cycles
  to verify the detector resets cleanly between turns.
