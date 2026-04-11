# claude-commander — Technical Debt & Complexity Analysis

*Principal-engineer code review, 2026-04-10. Reviewer: Claude (Opus 4.6).*

## TL;DR

The codebase is **~4,800 LOC of Rust** with clean module boundaries at the top level (`claude/`, `pty/`, `fs/`, `ui/`) but a severe **God Object** in `src/app.rs` (1,664 LOC — 34% of the binary crate). Rendering, input handling, state, PTY lifecycle, and file-tree mutation are all conflated in one struct with 34 public fields. Error handling is inconsistent — 14 `.unwrap()` calls and 14 `let _ =` sinks silently swallow PTY errors. Test coverage is **~0.3%** (236 LOC of tests vs. 4,840 LOC of source); the entire App state machine is untested. Clippy reports **32 warnings on default** and **115 on `pedantic`**. None of these are fatal, but together they indicate the project has outgrown its initial single-file design and needs a structural refactor before the next feature (the planned "fork" work in `PLAN.md`) lands on top of it.

---

## 1. Metrics

### LOC distribution

| Area            | LOC    | Share |
|-----------------|--------|-------|
| `app.rs`        | 1,664  | 34.4% |
| `ui/`           | 1,637  | 33.8% |
| `claude/`       |   475  |  9.8% |
| `fs/`           |   334  |  6.9% |
| `pty/`          |   235  |  4.9% |
| `setup.rs`      |   135  |  2.8% |
| `main.rs`       |   123  |  2.5% |
| `event.rs`      |    72  |  1.5% |
| **Total `src`** | **4,840** | |
| `tests/`        |   236  |  (≈ 0.3% effective coverage) |

### Clippy

- **Default lints:** 32 warnings.
- **`-W clippy::pedantic`:** 115 warnings. Top categories:
  - 42× missing `#[must_use]`
  - 19× `collapsible_if` / collapsible `if let`
  - 19× `cast_possible_truncation` (`usize as u16` in UI code)
  - 14× missing backticks in doc comments
  - 6× `uninlined_format_args`
  - 6× `redundant_closure_for_method_calls`
  - 5× `manual_let_else`
  - 4× `match_same_arms`
  - 3× `too_many_lines` (functions > 100 lines)
  - 2× `too_many_arguments`
  - 1× missing `Default` (`PromptDetector`)

### Panics / error-swallowing

- **14 `.unwrap()` sites.** 5 in `app.rs`, 5 in `pty/detector.rs` (compile-time regex, safe), 3 in `pty/session.rs`, 1 in `ui/panels/session_view.rs`.
- **14 `let _ =` sinks** — every PTY `write`/`resize` error is silently dropped.
- **Risky unwraps worth fixing immediately:**
  - `src/app.rs:307` and `:331` — `parser.lock().unwrap()` on a `Mutex` shared with the PTY reader thread. If that thread panics while holding the lock, the whole TUI dies on the next tick.
  - `src/pty/session.rs:85` — same mutex, inside the reader thread itself; a panic here takes the thread down with no restart and no user-visible signal.
  - `src/app.rs:894,906` — `path.strip_prefix(&home).unwrap()` will panic if the user opens a file outside `$HOME`.

---

## 2. The `app.rs` God Object (severity: HIGH)

`App` has **34 `pub` fields** and **40 methods** in a single 1,664-line file. The fields span at least eight distinct concerns:

| Concern                 | Example fields                                                            |
|-------------------------|---------------------------------------------------------------------------|
| Session lifecycle       | `sessions`, `selected`, `next_id`                                         |
| UI mode / focus         | `mode`, `focus`, `show_help`, `picker_selected`                           |
| Modal input state       | `new_session`, `input_buffer`, rename state                               |
| Editor buffer           | `editor`                                                                  |
| File tree               | `file_tree`, `file_tree_scroll`                                           |
| Terminal geometry       | terminal cols/rows, per-pane scroll offsets, `user_scrolled`              |
| Event plumbing          | event sender/receiver, PTY output routing                                 |
| Claude metadata         | usage stats, rate-limit, context percentages                              |

Every field is `pub`, so any caller can break invariants. Nothing enforces, e.g., that `selected < sessions.len()` or that `AppMode::SessionView(id)` refers to a live session. Direct indexing like `self.sessions[self.selected].label = ...` at `app.rs:736` will panic if that invariant is ever violated.

**Suggested seams** (roughly in order of payoff):

1. **`SessionManager`** — owns `Vec<Session>`, `selected`, `next_id`; exposes `spawn`, `kill`, `find_mut(id)`, `refresh_context`, `check_attention`. Removes ~7 repeated `sessions.iter_mut().find(|s| s.id == id)` patterns (app.rs:170, 418, 691, 719, 1217, …).
2. **`UiState`** — `mode`, `focus`, `show_help`, per-pane scroll, `user_scrolled`, modal state. Enforces transitions as methods rather than bare field writes.
3. **`EventDispatcher`** — replace the 5 `handle_*_key` functions and the 80-line `handle_mouse` with a trait-based dispatch keyed on `mode`. Today `handle_key` at `app.rs:230` and `handle_mouse` at `:282` both re-match on `AppMode` and duplicate bounds logic.
4. **Move rendering out of `app.rs`** — `draw()` (`app.rs:1093`, ~160 LOC with 8 `AppMode` arms) and `draw_setup_screen()` (`:1255`, ~80 LOC) construct ratatui widgets inline. The `app` module imports `ratatui::{text, style, layout, widgets}` in ~24 places; UI construction belongs in `ui/`.

---

## 3. Complexity hotspots

Top functions by structural complexity (match arms + nesting):

| Rank | Function                              | File:line          | LOC | Notes |
|------|---------------------------------------|--------------------|-----|-------|
| 1    | `App::draw`                           | app.rs:1093        | ~160| 8 `AppMode` arms + nested `if let` chains |
| 2    | `App::draw_setup_screen`              | app.rs:1255        | ~80 | clippy `too_many_lines`: 139/100 |
| 3    | `App::handle_mouse`                   | app.rs:282         | ~80 | 2× nested mode/focus matches |
| 4    | `CommandBar::render`                  | ui/panels/command_bar.rs:47 | 106 | clippy `too_many_lines` |
| 5    | `App::tab_complete_path`              | app.rs:849         | ~70 | string manipulation + I/O tangled |
| 6    | `App::handle_key`                     | app.rs:230         | ~50 | 7 mode branches + global filters |
| 7    | `App::handle_dashboard_key`           | app.rs:364         | ~32 | two sub-focus paths, 8 keys each |
| 8    | `App::spawn_from_modal`               | app.rs:823         | ~25 | validation + state + spawn mixed |
| 9    | `UsageGraph::render_usage_section`    | ui/panels/usage_graph.rs:121 | — | 9 parameters; clippy `too_many_arguments` |
| 10   | `pty::session::Session::spawn`        | pty/session.rs:39  | — | 8 parameters; PTY setup + reader thread |

All three functions exceeding clippy's `too_many_lines` threshold live in the two files that need to be split (`app.rs` and `ui/panels/command_bar.rs`).

---

## 4. Duplication

- **Session lookup by id** — the pattern `sessions.iter_mut().find(|s| s.id == session_id)` appears at `app.rs:170`, `:418`, `:691`, `:719`, `:1217`. Extract `SessionManager::get_mut(id) -> Option<&mut Session>`.
- **Nav-key handling** — `Up`/`Down`/`j`/`k` logic is re-implemented in `handle_session_list_key`, `handle_session_picker_key`, `handle_file_tree_key`, and the editor handler. A tiny `Navigable` helper or shared `saturating_prev / next` fn would remove ~40 lines.
- **Path-parent fallback** — `app.rs:488` uses `path.parent().unwrap_or(path)` while `app.rs:862` uses `path.parent().unwrap_or(Path::new("/"))`. Inconsistent fallback across the same file.
- **`format!`/`clone` in render paths** — `app.rs:1113` calls `.label.clone()` for every session every frame; many `format!("Rename: {}_", ...)` calls allocate per tick. Cheap to cache or pass borrowed slices.

---

## 5. Error handling

### Silent PTY failures

Every PTY write/resize in `app.rs` is discarded:

```
app.rs:223  let _ = session.resize(cols, rows);
app.rs:421  let _ = session.resize(cols, rows);
app.rs:658  let _ = session.write(bytes);
app.rs:692  let _ = session.write(bytes);
app.rs:722  let _ = session.resize(...);
app.rs:968  let _ = session.write(...);
app.rs:974  let _ = session.write(...);
app.rs:986  let _ = session.write(...);
```

If the PTY dies, the UI keeps rendering as though everything is fine. At minimum these should `log::warn!`; ideally they should mark the session as `SessionStatus::Exited(reason)` so the user sees the failure.

### Mutex-poison risk

`Session::parser: Arc<Mutex<vt100::Parser>>` is locked from both the UI thread (`app.rs:307,331,1217`) and the PTY reader thread (`pty/session.rs:85,131`). Every lock uses `.unwrap()`. If either side panics while holding the lock, every subsequent `.lock().unwrap()` panics the caller. A 3-line fix:

```rust
fn lock_parser(p: &Mutex<vt100::Parser>) -> MutexGuard<'_, vt100::Parser> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}
```

### Inconsistency

Library-ish modules (`claude/`, `fs/`, `pty/`) return `anyhow::Result<T>` rather than domain error types. For a binary crate this is fine, but if any of these modules grow a second consumer (e.g., tests, or the planned fork feature), `thiserror` enums would make the failure modes inspectable.

---

## 6. State management smells

- **Bool soup.** `should_quit`, `user_scrolled`, `show_help`, `setup_banner_dismissed` — some of these are fine, but `user_scrolled` in particular would be clearer as `enum ScrollMode { Auto, Pinned(usize) }`; today the scroll-restoration logic at `app.rs:174-178` has to correlate a bool with a separate offset field.
- **Unguarded indexing.** `self.sessions[self.selected]` appears at `app.rs:691`, `:718`, `:736`. Swap for `.get(self.selected)` or centralise access behind `SessionManager::selected_mut()`.
- **Stale `picker_selected`** after session removal — no invariant is maintained; if the picker is open when a session exits, the selection can dangle. Not observed in practice but trivially triggerable.
- **34 `pub` fields.** Any feature-branch can mutate any field. Make them `pub(crate)` or private and expose narrow accessors.

---

## 7. Module boundaries

The top-level split is genuinely clean: `claude/`, `pty/`, `fs/`, `ui/`, `event` all make sense as modules and don't cycle. The leak is **upward**: `app.rs` reaches down into every layer:

- 24 direct `ratatui::` imports (widget construction in the app layer).
- Direct calls into `Session::{resize,write}` mingled with UI state changes.
- Direct mutation of `FileTree` fields from event handlers, bypassing any invariants the type might want to enforce.

The fix isn't to add more layers — it's to push the current `draw_*` and `spawn_from_modal`-style helpers down into the modules that own the data.

---

## 8. Rust idiom nits

These are individually small but worth cleaning up because they add up:

- **Unnecessary clones.** `app.rs:436`, `:1113`, `:69`, `:736` clone `String` labels that could be borrowed. Estimate ~8–10 clones per render tick that aren't needed.
- **`String` parameters** where `&str` would do: `NewSessionState::with_dir(dir: String)` at `app.rs:56`.
- **`map(..).unwrap_or(..)`** instead of `map_or(..)` — 5 sites flagged by clippy.
- **Collapsible `if let` chains** — 19 sites. `cargo clippy --fix` cleans these up for free.
- **`usize as u16` casts** — 19 sites in UI code. Safe in practice but pedantic clippy is right that these should be `u16::try_from(...).unwrap_or(u16::MAX)` at the boundary.
- **Missing `Default`** for `PromptDetector` (`pty/detector.rs:17`) and `ThemeName` (`ui/theme.rs:29`).
- **Missing `Display`** for `SessionStatus` — logs and status bars currently rely on `{:?}`.
- **Functions > 7 args:** `Session::spawn` (8 params), `render_usage_section` (9 params). Bundle into a config struct each.
- **`unused_self`** on `FileTree::has_session_at` (`fs/tree.rs:179`) — should be an associated fn.
- **`struct_field_names`** on `UsageStats` (`ui/panels/command_bar.rs:9`) — all three fields end in `_pct`.

---

## 9. Concurrency

The architecture is intentionally sync with one `std::thread::spawn` per PTY for the reader loop (`pty/session.rs:72-97`). That's the right choice for a TUI with ~5–10 sessions — adding tokio here would be gratuitous. The real issues are in the existing thread:

1. **No panic handling.** A panic in `parser.lock().unwrap()` or `event_tx.send()` silently orphans the PTY; the session's output stops updating but `SessionStatus` still says `Running`. Wrap the loop body in `std::panic::catch_unwind` and emit a `SessionExited` event on panic.
2. **No graceful shutdown path.** The reader only exits on EOF. If the user closes a session via `K` while the child is still streaming, the thread keeps reading until the PTY pipe closes. Fine today; worth a `cancel: Arc<AtomicBool>` if sessions start getting reused.
3. **Unbounded event channel.** `event_tx.send(Event::PtyOutput { ... })` can back up if the main thread is slow (e.g., during a large paste). Consider `sync_channel` with a small buffer and drop-old semantics for `PtyOutput`.

---

## 10. Testing gaps

`tests/unit_tests.rs` is 236 LOC and covers:

- ✅ `fs::git` status parsing (3 tests)
- ✅ `fs::tree` navigation (3 tests)
- ✅ `pty::detector::PromptDetector` (4 tests)
- ✅ `ui::panels::editor::EditorState` (7 tests)

Entirely **untested**:

- `App` event handling — the entire state machine.
- Session lifecycle (spawn / kill / resize / write).
- Mode and focus transitions.
- Session lookup / `picker_selected` invariants.
- Rate-limit parsing (`claude/rate_limit.rs`) — has a fiddly JSON shape.
- `claude/usage.rs` parsing.

With the planned refactor into `SessionManager` and `UiState`, the first tests should be **mode-transition property tests** — they catch the most dangerous class of bug (invalid `AppMode::SessionView(id)`) for cheap.

---

## 11. Dependencies

`Cargo.toml` is lean; nothing jumps out as bloated. Two observations:

- `log` and `env_logger` are wired up but the codebase only makes a handful of `log::` calls. Either use logging more (see §5 — the PTY error swallows are prime candidates) or drop both.
- `chrono` is pulled in but mostly used for `Local.timestamp_opt(...)` formatting in `claude/rate_limit.rs`. If that's the only use, `jiff` or even manual formatting via `std::time::SystemTime` would cut the dependency.
- `ratatui` is pinned to 0.30; 0.31 is available and is a drop-in bump.
- No security advisories; `cargo audit` not run here but worth adding to CI.

---

## 12. Prioritised findings (top 20)

| #  | File:line                          | Severity | Finding                                                             | Fix |
|----|------------------------------------|----------|---------------------------------------------------------------------|-----|
| 1  | `app.rs` (whole file)              | HIGH     | God object: 1,664 LOC, 34 pub fields, 40 methods, 8 concerns        | Extract `SessionManager`, `UiState`, move `draw_*` into `ui/` |
| 2  | `app.rs:307,331,1217` + `pty/session.rs:85,131` | CRITICAL | `parser.lock().unwrap()` panics on mutex poison      | `.unwrap_or_else(\|e\| e.into_inner())` helper |
| 3  | `pty/session.rs:72`                | CRITICAL | PTY reader thread has no panic handling; silent death              | `catch_unwind` + emit `SessionExited(reason)` on panic |
| 4  | `app.rs:223,421,658,692,722,968,974,986` | HIGH | 8× `let _ = session.{write,resize}(...)` — silent PTY failures | `log::warn!` or mark session as exited |
| 5  | `app.rs:691,718,736`               | HIGH     | Unguarded `self.sessions[self.selected]` indexing                   | Use `.get(..)` or `SessionManager::selected_mut()` |
| 6  | `app.rs:894,906`                   | MEDIUM   | `path.strip_prefix(&home).unwrap()` panics outside `$HOME`          | Handle `Err` with a graceful fallback |
| 7  | `app.rs:1093`                      | HIGH     | `draw()` ~160 LOC, 8 `AppMode` arms                                 | Split per-mode into `ui/draw/<mode>.rs` |
| 8  | `app.rs:1255`                      | MEDIUM   | `draw_setup_screen` clippy `too_many_lines` 139/100                 | Move to `ui/setup.rs`, split by section |
| 9  | `ui/panels/command_bar.rs:47`      | MEDIUM   | `CommandBar::render` 106 LOC                                        | Split render into header/status/usage helpers |
| 10 | `app.rs:170,418,691,719,1217`      | MEDIUM   | Repeated `sessions.iter_mut().find(\|s\| s.id == id)`               | `SessionManager::get_mut(id)` |
| 11 | `app.rs:364..` handlers            | MEDIUM   | Nav-key logic duplicated across 4 handlers                          | Shared `Navigable` helper |
| 12 | `app.rs:80-114`                    | MEDIUM   | All 34 App fields `pub`                                             | Downgrade to `pub(crate)` / private + accessors |
| 13 | `pty/session.rs:39`                | LOW      | `Session::spawn` has 8 args                                         | Bundle into `PtyConfig { cols, rows, cwd, ... }` |
| 14 | `ui/panels/usage_graph.rs:121`     | LOW      | `render_usage_section` has 9 args                                   | Bundle into `UsageCtx` |
| 15 | `app.rs` (24 sites)                | MEDIUM   | `ratatui::*` imports in app layer                                   | Push widget construction into `ui/` |
| 16 | `app.rs:622`                       | MEDIUM   | `send_file_key` parses digits without bounds-checking session index | `if idx < sessions.len()` |
| 17 | `pty/detector.rs:17`               | LOW      | Missing `Default` for `PromptDetector`                              | `impl Default` |
| 18 | `ui/theme.rs:29`                   | LOW      | `ThemeName::ALL.iter().position(...).unwrap_or(0)`                  | `impl Default for ThemeName` |
| 19 | `fs/tree.rs:179`                   | LOW      | `has_session_at(&self, ...)` — `unused_self`                        | Make associated fn |
| 20 | `tests/unit_tests.rs`              | HIGH     | ≈0.3% coverage; no tests for `App` state machine                    | Start with mode-transition tests after §1 refactor |

*(Full clippy list — 32 default, 115 pedantic — is reproducible via `cargo clippy --all-targets -- -W clippy::pedantic`; 22 warnings are auto-fixable with `cargo clippy --fix`.)*

---

## Recommended order of attack

1. **Stop the panics.** Fix findings #2 and #3 (mutex poison, PTY thread panic). ~1 hour, no architectural changes.
2. **Stop the silent failures.** Replace `let _ =` PTY calls with logged errors (#4). ~1 hour.
3. **Extract `SessionManager`.** Pull session lookup/lifecycle out of `app.rs` (#1 partial, #10, #16). ~1 day. Unblocks testing.
4. **Add `SessionManager` tests.** First time the state machine is testable. ~½ day.
5. **Move rendering into `ui/`.** Split `draw()` and `draw_setup_screen` (#7, #8, #15). ~1 day.
6. **Clippy sweep.** `cargo clippy --fix` for the auto-fixables, then manually address `too_many_lines`, `too_many_arguments`, and missing `Default`/`Display`. ~½ day.
7. **Decide on `log`/`chrono`.** Either commit to logging or drop both. ~1 hour.

Steps 1, 2, and 6 are near-zero-risk and can ship independently. Steps 3–5 are the real refactor and should land before the "fork" feature in `PLAN.md` — adding fork-on-top-of-session semantics to the current `app.rs` will make it significantly harder to refactor later.
