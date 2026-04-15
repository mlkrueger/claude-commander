# Mouse scroll fixes

This note records three scroll bugs fixed on the `main` branch in April 2026
and one follow-up still open. Each section explains what the user saw, the
root cause, and the fix so we don't re-learn the same lessons next time
someone touches `src/app/keys.rs::handle_mouse`.

## Bug 1 — Wheel scrolled the outer terminal window

**Symptom.** Scrolling the mouse wheel anywhere inside ccom would also scroll
the host terminal (iTerm2 / macOS Terminal), meaning the whole ccom TUI slid
out of view.

**Cause.** ccom was enabling mouse tracking via crossterm's
`EnableMouseCapture`, which emits five DEC private mode sequences — including
`?1003h` (any-event / all-motion tracking). Several terminals treat `?1003h`
as "unsupported" and respond by silently disabling *all* mouse tracking on
that connection. With no mouse tracking, wheel events bubble up to the host
terminal and scroll its scrollback instead of reaching ccom.

**Fix.** `src/main.rs` now writes a narrower set of modes by hand:

```
?1000h  — normal tracking (press/release, includes wheel)
?1002h  — button-event tracking (required by iTerm2/Kitty/WezTerm to deliver wheel)
?1006h  — SGR extended coordinates
```

We deliberately skip `?1003h` and `?1015h`. This is also re-asserted after
every resize event because some terminals reset mouse mode on SIGWINCH.

## Bug 2 — Alt+M didn't actually toggle text selection

**Symptom.** Users need to disable mouse capture briefly to select + copy
text out of ccom. Toggling off didn't appear to work; text selection still
fought with ccom mouse handling.

**Cause.** The disable path wasn't emitting the *inverse* sequences of the
enable path, so the terminal remained in some partial mouse-tracking state.

**Fix.** `src/main.rs` now writes `?1006l ?1002l ?1000l` (disable in reverse
order of enable) when toggling capture off, and the full enable triplet when
toggling back on.

## Bug 3 — Scrolling inside a SessionView did nothing

**Symptom.** With mouse capture correctly scoped to ccom, wheel events inside
a Claude Code SessionView did nothing. Users couldn't scroll back through
transcripts.

This was the painful one. Three wrong assumptions were stacked on top of
each other:

### Wrong assumption 1: "Claude Code uses alternate-screen mode"

The original SessionView scroll handler forwarded a fabricated SGR mouse
escape into the child PTY (`ESC [ < 64 ; col ; row M`) with the comment:

> vt100 scrollback doesn't work here because Claude Code uses alternate
> screen mode (alternate_grid has scrollback_len=0). Claude Code handles its
> own scroll natively when it receives mouse wheel events.

Neither clause was verified. We added a diagnostic key (**Alt+G**) in
`src/app/keys.rs::handle_session_view_key` that samples the session's
`vt100::Screen` and prints:

```
alt_screen={bool} mouse_mode={Mode} mouse_enc={Enc} sb_rows={N} sb_offset={N}
```

Running it inside a live Claude Code session showed
`alt_screen=false mouse_mode=None sb_rows=42` after some transcript activity.

So in reality:

1. Claude Code runs on the **primary screen**, not the alternate screen, and
   its output *does* scroll lines off the top.
2. The vt100 parser's primary scrollback buffer fills correctly — up to the
   1000-row cap set in `Session::new`.
3. Claude Code never requests mouse tracking (`mouse_mode=None`), so any SGR
   escapes we forward into its stdin are garbage input, not scroll events.

### Wrong assumption 2: "The existing forwarding path worked for pagers"

Someone testing `man ls` (which drops into `less` on macOS) had previously
seen "scrolling work" under the old code. That turned out to be an illusion.

Probe inside `less`: `alt_screen=true mouse_mode=None sb_rows=0`. So less
really does use alt-screen and really doesn't listen for mouse. The old
forwarding code was writing `ESC [ < 64 ; col ; row M` into less's stdin,
and less was interpreting those bytes as a partial command sequence: the
`ESC [` put less into a command-read state, the trailing `M` triggered the
"mark current position" command, and subsequent wheel events bounced the
command-line prompt on and off screen with ephemeral `ESC [` and `:`
artifacts that the next redraw hid.

We weren't scrolling. We were silently injecting marks and garbage into
pagers every time the user touched the wheel.

### Wrong assumption 3: "`session_view_scroll` was driving the render"

`src/ui/panels/session_view.rs` already called
`parser.screen_mut().set_scrollback(self.scroll_offset)` with a value plumbed
from `App::session_view_scroll`. But grep showed that field was only ever
*reset* to zero — never *incremented*. The plumbing existed; nobody pulled
the lever. The scroll handler was bypassing this entirely in favour of the
broken PTY-forwarding path.

### The fix

`src/app/keys.rs::handle_mouse` now branches per-event on
`screen.mouse_protocol_mode()`:

```text
if mouse_mode != None           → forward SGR wheel escape (mouse-aware
                                   programs: tmux, htop, vim with `:set mouse=a`)
else                            → adjust session_view_scroll locally against
                                   the vt100 primary-screen scrollback
```

The clamp in the scroll-up branch uses a trick: vt100 has no public API for
"how many rows of scrollback do you have?", but `set_scrollback` clamps to
the underlying `VecDeque::len()` internally, so probing with `usize::MAX`
and reading back the resulting `scrollback()` gives us the real row count.
(The Alt+G diagnostic uses the same trick.)

### Test matrix at fix time

| Case | `alt_screen` | `mouse_mode` | Behavior |
|------|--------------|--------------|----------|
| Claude session after activity | `false` | `None` | Local scrollback (fix path). ✅ |
| `fish` + `cat Cargo.lock` | `false` | `None` | Local scrollback. ✅ |
| `htop` / `tmux` | `true` | `ButtonMotion` | Forwarded SGR. ✅ |
| `vim` (default, no `:set mouse=`) | `true` | `None` | No-op (matches native vim). ✅ |
| `less Cargo.lock` / `man ls` | `true` | `None` | No-op, no garbage injection. ✅ |

### Lesson

Three "obvious" assumptions (alt-screen, Claude handles mouse, `less`
already scrolled) would all have been disproved in 90 seconds by a screen
probe. Before writing code to route around a vt100 limitation, dump
`alternate_screen()`, `mouse_protocol_mode()`, and scrollback length. The
Alt+G diagnostic is kept in the tree for exactly this reason.

---

## Open follow-up — File tree wheel scroll is erratic

Wheel events over the file tree pane don't scroll the viewport smoothly;
they skip in 1-row jumps even though the wheel delivers 3-line bursts.

**Cause.** `src/app/keys.rs::handle_mouse` does
`for _ in 0..3 { file_tree.move_up() }` followed by
`adjust_file_tree_scroll()`. `move_up`/`move_down` move the **selection
cursor**, not the viewport. `adjust_file_tree_scroll` only shifts
`file_tree_scroll` when the selection would otherwise leave the visible
window — it clamps the cursor back into view by the minimum amount.

User-visible effect: if the cursor sits in the middle of the viewport, three
wheel ticks walk the cursor to the edge without moving the viewport at all,
then subsequent ticks each bump the viewport by one row. It reads as
"static, static, static, step, step, step".

**Proposed fix.** Wheel should scroll the viewport independently of
selection — adjust `file_tree_scroll` directly, clamped to
`[0, visible_cache.len() - visible_height]`, without touching `selected`.
That matches every other pane (session list, SessionView, editor).

Out of scope for the SessionView-scroll PR; filed here for a future branch.
