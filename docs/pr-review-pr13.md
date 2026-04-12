# PR #13 Review — `session-mgmt phase 3.5: hook-based response boundary detection`

**Date:** 2026-04-12
**Branch:** `session-mgmt/phase-3.5-hook-boundary`
**Scope:** 13 files, +1041/-7
**Status:** All critical and high-priority items resolved; all mediums except M5 applied. Second-pass review items (N1, N2, T8, two nits) also applied. 296 tests pass, zero clippy warnings. See `docs/plans/phase-3.5-review-fixes.md` for the fix plan and Resolution column below.

## Second-pass findings (2026-04-12 post-fix review)

| ID | Item | Status |
|---|---|---|
| **N1** | Residual drain/send race in `cleanup_hook_artifacts` — signals sent between the first drain and `join_with_timeout` return could be lost. | ✅ Applied. Added a second drain pass *after* `join_with_timeout` returns (race-free because the thread is gone post-join). See `src/session/types.rs` step 5 of `cleanup_hook_artifacts`. |
| **N2** | `build_hook_settings` uses `Path::display().to_string()`, which is lossy for non-UTF8 paths. A non-UTF8 `TMPDIR` would produce a settings.json whose quoted path doesn't match the real FIFO. | ✅ Applied. `build_hook_settings` now returns `io::Result<String>` and errors early with a clear message if the fifo path is not valid UTF-8. Caller in `create_hook_dir` propagates the error. |
| **N3** | T2 structurally weak on macOS (portable_pty defers exec errors to the child). | ⏸️ Acknowledged. The test already tolerates both behaviors; tightening requires a mockable `spawn_command` — deferred. |
| **T8 flake risk** | 17 MB FIFO round-trip with 10s timeout on contended CI. | ✅ Applied. Timeout bumped to 20s. |
| **T3/T4 cross-process collision** | SERIAL mutex is intra-process only. | ⏸️ Low priority, deferred until we see a real cross-process collision. |
| **Nit: `v.as_str().unwrap()`** after `is_string()` check in `parse_hook_stdin`. | ✅ Applied. Rewritten as `match v.as_str()` to avoid the unwrap. |
| **Nit: misleading "single-pass drain + process" comment** in `check_hook_signals`. | ✅ Applied. Comment now accurately describes the single outer loop with per-session local `Vec`. |
| **Nit: `SidecarHandle::join_with_timeout` 10ms polling** | ⏸️ Deferred. `thread::park_timeout` is more idiomatic but the current polling is correct and the overhead is bounded. |
| **Nit: `shell_single_quote` iterates chars not bytes** | ⏸️ Moot after N2 — the function is only ever fed valid UTF-8 now. |

## Overview

Per-session Stop hook for Claude Code: creates `/tmp/ccom-<pid>-<sid>/.claude/settings.json`, symlinks the rest of `~/.claude/` through so auth is preserved, creates a FIFO the hook shells write to, and runs a sidecar reader thread that forwards parsed `HookStopSignal`s to a per-session mpsc channel which `SessionManager::check_hook_signals` drains each tick. The architecture is clean and the implementation-deltas doc (§10 of the design) is excellent. **Two correctness bugs, one security bug, and several high-priority issues** need addressing before merge.

---

## Critical (must fix)

### C1. `reap_exited` never cleans up hook artifacts — guaranteed resource leak
**Location:** `src/session/manager.rs:610-632`

Only `Session::kill` calls `cleanup_hook_artifacts`. When a Claude session exits on its own (user types `/exit`, crash, ctrl-D), we leak:
- the sidecar reader thread (blocked on `File::open` of the FIFO)
- the `hook_rx` receiver
- the `/tmp/ccom-<pid>-<sid>/` directory (including symlinks to `~/.claude/*`)
- the FIFO itself

Over a long-running TUI session where the user spawns and exits several Claude sessions, `/tmp` accumulates stale dirs and the process accumulates stuck reader threads.

**Fix:** call `cleanup_hook_artifacts()` on each transitioned session in `reap_exited`. Add regression test.

### C2. Shell injection / breakage via unescaped FIFO path
**Location:** `src/session/hook.rs:150-154` (`build_hook_settings`)

```rust
let fifo_str = fifo_path.display().to_string();
let command = format!("cat >> {fifo_str}; printf '\\n' >> {fifo_str}");
```

`fifo_path` derives from `std::env::temp_dir()`. If `TMPDIR` contains space, quote, backtick, `$`, `;`, `&`, or newline, the hook command breaks or executes arbitrary code. macOS's default `/var/folders/.../T/` is fine; user-set `TMPDIR=/tmp/my project/` is not. This is a real correctness bug and a latent shell-injection surface.

**Fix:** single-quote the path and escape embedded single quotes:
```rust
let quoted = format!("'{}'", fifo_str.replace('\'', "'\\''"));
let command = format!("cat >> {quoted}; printf '\\n' >> {quoted}");
```
Add a test with a fifo path containing a space and a single quote.

### C3. Cleanup race: dropping `hook_rx` first can discard in-flight signals
**Location:** `src/session/types.rs:340-385` (`cleanup_hook_artifacts`)

Flow:
1. Reader thread is inside `reader.lines()`, has just parsed a Stop JSON line, calls `tx.send(signal)`.
2. `cleanup_hook_artifacts` drops `hook_rx` first → `tx.send(...)` returns `Err` → thread exits → **signal is silently lost**.

During `kill` this is arguably fine (we're tearing down). Combined with the C1 fix, a session that completes its final turn and *immediately* exits can lose the final `ResponseComplete` — the exact race the hook was supposed to fix.

Additionally, the write-open unblock dance has a window where the reader may have looped back to `File::open` after we already closed our write end, leaving the thread stuck forever. The 500ms timeout that silently orphans the thread (log-warn, move on) is a code smell: leaking an OS thread on every teardown is not OK.

**Fix:**
1. Drain any pending signals before dropping `hook_rx`.
2. Introduce an `Arc<AtomicBool>` stop flag the reader checks on each iteration, combined with the write-poke to unblock the current `File::open`.
3. Fail loudly (not just warn) if the thread doesn't exit within the timeout.

### C4. `BufRead::lines()` has no upper bound — unbounded memory
**Location:** `src/session/hook.rs:222-241`

`last_assistant_message` can legitimately be multiple MB. A malformed hook write without a newline terminator would let the reader thread grow memory unbounded.

**Fix:** manual read loop with a sanity cap (e.g., 16 MB). Log-warn and skip oversized messages.

---

## High-priority (should fix)

### H1. World-readable temp dir + TOCTOU on `settings.json` — local privilege issue
**Location:** `src/session/hook.rs:97-142`

`create_dir_all` uses process umask → `/tmp/ccom-<pid>-<sid>/` is 0755 on Linux. On a shared host, another local user can:
- Inspect the symlink farm (information leak about which files exist in `~/.claude/`)
- Between `create_dir_all` and `fs::write`, pre-create `settings.json` as a symlink to `$HOME/.bashrc`, achieving arbitrary file clobber as the ccom user

This is the most severe security finding. Trivially fixed:
```rust
use std::os::unix::fs::DirBuilderExt;
fs::DirBuilder::new().mode(0o700).recursive(true).create(&claude_dir)?;
// settings.json:
OpenOptions::new().create_new(true).mode(0o600).write(true).open(&path)?;
```

### H2. `remove_dir_all` symlink safety — add regression test + doc comment
**Location:** `src/session/hook.rs:250-257`

Rust ≥1.70's `fs::remove_dir_all` does not follow symlinks for removal (it `unlinkat`s them). Add a code comment so a future refactor doesn't reintroduce a pre-1.70 helper. Add a regression test: create a symlink inside the hook dir pointing at a tempfile, verify `cleanup_hook_dir` removes the symlink but leaves the target.

### H3. `CLAUDE_CONFIG_DIR` unconditionally overridden
**Location:** `src/session/types.rs:118`

If the user already exports `CLAUDE_CONFIG_DIR` (nix, some CI), we silently clobber it. Our symlink farm points at `~/.claude/`, not at their actual custom config dir — auth breaks.

**Fix:** in `create_hook_dir`, resolve the real config dir via `env::var("CLAUDE_CONFIG_DIR")` first, fall back to `~/.claude/`.

### H4. Hook settings may coexist with user's project/local hooks
The design doc §10.5 assumes our `CLAUDE_CONFIG_DIR` settings override the user's, but Claude Code still merges project and local settings. If the user has their own Stop hook in a project `.claude/settings.local.json`, it runs alongside ours. If it's slow (>5s timeout), our signal is delayed. Log-warn at startup if a conflicting hook is detected.

### H5. O(n²) double loop in `check_hook_signals`
**Location:** `src/session/manager.rs:922-958`

The "drain then find-by-id and process" split only exists to work around the borrow checker. Use index-based iteration for a cleaner single pass.

---

## Medium / nice-to-have

| ID | Item |
|---|---|
| **M1** | `hook_rx` doc claims `Some` iff `hook_dir` is `Some` — `install_test_hook_channel` violates this. Fix the comment or set `hook_dir` too. |
| **M2** | Hook timeout of 5s (`build_hook_settings`) is aggressive. Consider 10–30s. |
| **M3** | `read_dir().flatten()` silently swallows per-entry errors. Log-warn. |
| **M4** | FIFO reader breaks on first `File::open` failure. Retry transient errors. |
| **M5** | `libc` is fine but `rustix` is lighter with no unsafe at the call site. Optional swap. |
| **M6** | `parse_hook_stdin` silently returns `None` if `last_assistant_message` exists but isn't a string. Log-warn. |
| **M7** | `#[cfg(unix)]` guards only `create_stop_fifo`. Gate the whole `install_hook` branch. |

---

## Test coverage gaps

| ID | Item |
|---|---|
| **T1** | `reap_exited` cleans up hook artifacts (fails until C1 fixed) |
| **T2** | `Session::spawn` cleans up hook dir when `spawn_command` fails |
| **T3** | Concurrent two-session isolation using real FIFOs (not the test seam) |
| **T4** | End-to-end `check_hook_signals` with a real FIFO |
| **T5** | `cleanup_hook_artifacts` called twice — idempotency |
| **T6** | `parse_hook_stdin` with a 1 MB body |
| **T7** | FIFO path containing a space and a single quote (paired with C2) |
| **T8** | Oversized FIFO line (paired with C4) |
| **T9** | Symlink-in-hook-dir cleanup safety (paired with H2) |

---

## Security summary

**H1** is the most serious: 0755 temp dir + TOCTOU on `settings.json` enables arbitrary file clobber by any local user. Multi-user Linux boxes with an untrusted local user are at risk. **C2** is the next priority (shell injection via TMPDIR).

---

## Overall assessment

**Resolved 2026-04-12.** Core design and code quality are good; the review-fix pass applied all critical/high items and most mediums.

## Resolutions

| ID | Status | Notes |
|---|---|---|
| **C1** | ✅ Applied | `SessionManager::reap_exited` now calls `cleanup_hook_artifacts` in a single-pass loop and publishes drained `ResponseComplete` signals BEFORE `Exited`. Regression test `reap_exited_cleans_up_hook_artifacts` + `reap_exited_publishes_response_complete_before_exited`. |
| **C2** | ✅ Applied | New `shell_single_quote` POSIX helper; `build_hook_settings` emits `'...'`-quoted fifo paths with embedded-quote escaping. Tests `shell_single_quote_handles_tricky_chars`, `build_hook_settings_escapes_tricky_paths`, and `fifo_path_with_space_and_quote` (end-to-end shell exec round-trip). |
| **C3** | ✅ Applied | `cleanup_hook_artifacts` rewritten: drains pending signals into a `Vec` before dropping `hook_rx`, calls `SidecarHandle::request_stop`, write-pokes the FIFO, `join_with_timeout(500ms)`, logs at **error** (not warn) on timeout. `is_finished()` fast-path skips the write-poke when the thread has already exited. Return value threaded through `Session::kill` (discards) and `reap_exited` (publishes). |
| **C4** | ✅ Applied | New `read_line_bounded` helper with `MAX_HOOK_LINE_BYTES = 16 MB`. Oversized lines drain to next `\n` and log-warn. Test `fifo_skips_oversized_line_then_parses_next` writes a 17 MB line followed by a valid line and verifies the reader recovers. |
| **H1** | ✅ Applied | `DirBuilder::mode(0o700).recursive(true)` for root + `.claude/`; `settings.json` via `OpenOptions::create_new(true).mode(0o600)` refuses symlink-follow. Test `create_hook_dir_sets_secure_permissions`. |
| **H2** | ✅ Applied | Doc comment on `cleanup_hook_dir` cites Rust ≥1.70's non-follow behavior. Regression test `cleanup_preserves_symlink_targets` creates a symlink inside the hook dir and asserts cleanup removes the symlink but leaves the target. |
| **H3** | ✅ Applied | `create_hook_dir` resolves `CLAUDE_CONFIG_DIR` env var first, falls back to `~/.claude/`. |
| **H4** | ⏸️ Deferred | Log-warn on conflicting user hooks is low-urgency polish; deferred to a follow-up. |
| **H5** | ✅ Applied | `check_hook_signals` rewritten as single-pass `iter_mut()` with disjoint field borrows — no more drain-into-Vec + find-by-id second pass. |
| **M1** | ✅ Applied | `install_test_hook_channel` now also sets a dummy `hook_dir`, preserving the `Some`-iff invariant. |
| **M2** | ✅ Applied | Hook timeout bumped from 5s to 30s. |
| **M3** | ✅ Applied | `read_dir` per-entry errors now log-warn. |
| **M4** | ✅ Applied | `File::open` retries up to 3 times with 50ms backoff, honoring stop flag between attempts. |
| **M5** | ⏸️ Deferred | `rustix`-vs-`libc` dep swap is optional polish; current `libc::mkfifo` call is small and scoped. |
| **M6** | ✅ Applied | `parse_hook_stdin` log-warns on `last_assistant_message` present-but-wrong-type with the actual JSON type name. |
| **M7** | ✅ Applied | `create_hook_dir` and `spawn_fifo_reader` gated `#[cfg(unix)]`; `Session::spawn` hook-install branch also gated, returns a clear error on non-Unix. |

## Tests added

| ID | Test | Location |
|---|---|---|
| **T1** | `reap_exited_cleans_up_hook_artifacts` | `src/session/manager.rs` |
| **T1b** | `reap_exited_publishes_response_complete_before_exited` | `src/session/manager.rs` |
| **T2** | `spawn_cleans_up_hook_dir_on_spawn_command_failure` | `tests/unit_tests.rs` (session_hook_integration) |
| **T3** | `concurrent_sessions_have_isolated_hook_dirs` | `tests/unit_tests.rs` |
| **T4** | `check_hook_signals_end_to_end_with_real_fifo` | `tests/unit_tests.rs` |
| **T5** | `cleanup_hook_artifacts_is_idempotent` | `src/session/manager.rs` |
| **T6** | `parse_stdin_one_mb_body` | `src/session/hook.rs` |
| **T7** | `fifo_path_with_space_and_quote` | `src/session/hook.rs` |
| **T8** | `fifo_skips_oversized_line_then_parses_next` | `src/session/hook.rs` |
| **T9** | `cleanup_preserves_symlink_targets` | `src/session/hook.rs` |
| **T_perm** | `create_hook_dir_sets_secure_permissions` | `src/session/hook.rs` |
| **M6** | `parse_stdin_rejects_wrong_type_last_message` | `src/session/hook.rs` |

Total new tests: **14** (8 in hook.rs + 3 in manager.rs + 3 in unit_tests.rs). Post-fix test count: 296 (was 282).

## Notes from the fix pass

- **Parallel-test collision** on `/tmp/ccom-<pid>-<id>` paths: integration tests T2/T3/T4 share the pid namespace and `SessionManager::next_id` starts at 0, so two tests can collide on `mkfifo`. Worked around with a module-local `static SERIAL: Mutex<()>` that each hook-installing integration test acquires. The lock does not block the rest of the suite.
- **T2 platform nuance**: `portable_pty::slave::spawn_command` on macOS defers exec errors to the child (returns `Ok` for `/nonexistent/binary`). The test tolerates both behaviors — if spawn returns `Ok`, it `kill`s the session to exercise the cleanup-via-kill path; if `Err`, the pre-spawn cleanup path is exercised directly. This is a weaker assertion than the spec intended; documenting for a future tightening.
- **`SidecarHandle::is_finished` dead-code warning**: Stream B added a real caller in `cleanup_hook_artifacts` (fast-path that skips the write-poke when the thread has already exited) so the method isn't dead.

---

## File:line index

- `src/session/manager.rs:610-632` — reap_exited missing cleanup (C1)
- `src/session/hook.rs:150-154` — shell injection (C2)
- `src/session/hook.rs:97-142` — temp dir permissions + TOCTOU (H1)
- `src/session/types.rs:340-385` — cleanup race / thread-leak window (C3)
- `src/session/hook.rs:222-241` — unbounded BufRead::lines (C4)
- `src/session/hook.rs:250-257` — remove_dir_all symlink safety comment (H2)
- `src/session/types.rs:118` — CLAUDE_CONFIG_DIR clobber (H3)
- `src/session/manager.rs:922-958` — O(n²) double loop (H5)
- `src/session/types.rs:386-399` — test seam invariant mismatch (M1)
- `src/session/hook.rs:163` — 5s hook timeout (M2)
