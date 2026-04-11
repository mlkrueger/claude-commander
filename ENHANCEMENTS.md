# Enhancements

## Scroll position drifts during streaming (follow-up to the fix below)

The previous "scroll reset to 0 on every PtyOutput" bug is fixed — you
can now scroll up while a response is streaming. But there's a subtler
issue left: the scroll offset is stored as a line index `x`, and while
generation is running, line `x` refers to a *different piece of content*
every time a new line is appended. So the content you were looking at
drifts upward out of view as new output arrives.

**Desired behavior:**

- If the user is pinned to the bottom (viewing the prompt box) when
  generation starts, the existing auto-scroll-at-bottom behavior is
  correct — content scrolls up normally as it's generated.
- If the user is scrolled up (`x > 0`) when generation starts, the
  visible content should appear *static* — generation should look like
  it's happening "below" the viewport. Concretely: for each new line
  appended to the buffer during generation, bump `x` by 1 so the same
  content line stays anchored at the same screen row.

**Likely fix:** when a PtyOutput event arrives for the viewed session
and `user_scrolled` is set, compute the number of new lines added to
the parser buffer since the last event and add that delta to
`session_view_scroll` before rendering. Clear the delta tracking when
the user explicitly jumps to bottom / clears the scroll flag.

## Scroll position lost during active output

When viewing a session, scrolling up is immediately reset to the bottom on every
new PtyOutput event (`app.rs` ~line 128). This makes it impossible to scroll back
through output while Claude is actively working.

**Root cause:** `session_view_scroll` is unconditionally reset to 0 on every
PtyOutput for the viewed session.

**Possible fix:** Track whether the user has manually scrolled up (e.g. a
`user_scrolled` flag set on scroll-up key, cleared when the user explicitly
scrolls to bottom or presses a "follow" key). Only auto-scroll when the user
is already at the bottom. This is the pattern most terminal emulators use.

## Command bar too crowded

The dashboard command bar shows all shortcuts in a single row. As more features
are added this line overflows and becomes unreadable on smaller terminals.

**Proposed fix:** Keep only the most essential shortcuts visible in the command
bar (e.g. `n` new, `Enter` view, `q` quit, `Ctrl+H` help). Add a `Ctrl+H`
keybinding that opens a help modal listing all available shortcuts organized by
section (session management, file tree, navigation, etc.). This mirrors the
pattern used by tools like `htop` and `vim`.

## Stats panel (cost / tokens / break-even tracker)

A new panel surfacing today's total cost, per-session costs, total token
usage, and a "break-even" tracker comparing API-equivalent spend against a
configurable monthly subscription price. Also: day-over-day trends. Scoped to
the local host.

**Status:** Design complete, parked. Unclear whether this crosses the "wow,
this made my life better" bar for a general user, or whether it's mostly a
neat toy for power users curious about their spend.

**Full design doc:** [`docs/designs/stats-panel.md`](docs/designs/stats-panel.md)

The design doc covers: the constraint that Claude Code doesn't persist cost
in transcripts (only `usage` tokens + `model`), the two viable approaches
(parse-and-price vs capture-live-via-statusline-hook), why we'd pick the
live-capture approach, a delta-based storage model that handles multi-day
sessions and resume cleanly, the open empirical question about how Claude
Code's resume affects cost counters, and an implementation checklist.
