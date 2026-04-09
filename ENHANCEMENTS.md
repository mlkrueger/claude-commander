# Enhancements

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
