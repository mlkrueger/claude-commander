# Phase 3.5 Review Fixes — Implementation Plan

**Scope:** Address all concerns from `docs/pr-review-pr13.md`.
**Branch:** `session-mgmt/phase-3.5-hook-boundary` (same branch as the PR under review).

## Context

The Phase 3.5 PR (#13) introduced hook-based response boundary detection. The review identified 4 critical, 5 high, and 7 medium issues plus 9 test coverage gaps. This plan groups them into parallelizable work streams, executes them, and closes the review.

## Parallelization strategy

The fixes split cleanly into two groups by file, which lets us use two worktree subagents in parallel:

- **Stream A — `src/session/hook.rs`** (all fixes concentrated here): C2 shell escape, C4 bounded reads, H1 permissions, H2 symlink test, H3 env lookup, M2 timeout, M3/M4/M6 error logging, M7 cfg gate. Also part of C3 (reader-side AtomicBool stop flag).
- **Stream B — `src/session/manager.rs` + `src/session/types.rs`** (cleanup & dispatch): C1 reap_exited cleanup, H5 single-pass loop, and the `types.rs` side of C3 (drain-before-drop + improved thread join).

Stream A and Stream B touch disjoint files (except for C3, which spans both). We'll do C3's `hook.rs` side in Stream A and its `types.rs` side in Stream B, with a small coordination point: both sides agree on the `AtomicBool` stop-flag shape (defined in `hook.rs`, imported in `types.rs`).

After both streams complete, a third pass fixes the medium items that touch `manager.rs` test seam (M1) and adds cross-file regression tests (T1–T9) that exercise both sides.

## Stream A — `hook.rs` hardening

**Delegate:** subagent with the Explore/Edit tools, working in the current checkout.

### Tasks

1. **C2 (shell escape):** In `build_hook_settings`, quote the fifo path with single quotes and escape embedded single quotes per POSIX conventions. Unit test with paths containing space, single quote, and `$` / backtick.

2. **C4 (bounded reads):** Replace `BufReader::lines()` in `spawn_fifo_reader` with a manual read loop that caps line length at 16 MB. Skip oversized lines with a `log::warn!`.

3. **H1 (permissions + TOCTOU):** Replace `fs::create_dir_all(&claude_dir)?` with a `DirBuilder` that sets mode 0700 on both the root dir and the `.claude` subdir. Replace `fs::write(...)` for settings.json with `OpenOptions::new().create_new(true).mode(0o600).write(true)` to refuse following a pre-created symlink.

4. **H2 (symlink cleanup safety):** Add a code comment on `cleanup_hook_dir` explaining that `remove_dir_all` on Rust ≥1.70 does not follow symlinks. Add a regression test: create a symlink inside the hook dir → cleanup → target file unharmed.

5. **H3 (env lookup):** In `create_hook_dir`, resolve the user's real config dir via `env::var("CLAUDE_CONFIG_DIR").ok().map(PathBuf::from).or_else(|| dirs::home_dir().map(|h| h.join(".claude")))`.

6. **C3 (stop flag, reader side):** Add `pub struct SidecarHandle { stop: Arc<AtomicBool>, handle: JoinHandle<()> }` returned by `spawn_fifo_reader` instead of a raw `JoinHandle`. The reader thread checks `stop.load(Relaxed)` on each iteration of the outer `loop { File::open(...) }`. Stream B will call `handle.stop()` from `cleanup_hook_artifacts`.

7. **M2:** Bump hook `"timeout"` from 5 to 30 seconds in `build_hook_settings`.

8. **M3, M4, M6:** Log-warn on `read_dir` per-entry errors; retry `File::open` failures up to 3 times with a short backoff before breaking the outer loop; log-warn on `last_assistant_message` present-but-wrong-type in `parse_hook_stdin`.

9. **M7 (cfg gate):** Gate the entire `install_hook` branch in `Session::spawn` with `#[cfg(unix)]` — actually, do this in Stream B because it's in `types.rs`. Stream A only gates the `create_hook_dir` / `spawn_fifo_reader` functions (already done for `create_stop_fifo`; extend to the others).

### Tests added in Stream A
- **T6** 1 MB body round-trip through `parse_hook_stdin`
- **T7** fifo path with space + single quote still fires
- **T8** oversized line (>16 MB) is skipped with warning, subsequent lines still parse
- **T9** symlink inside hook dir is unlinked but target is preserved
- **T_perm** created hook dir has mode 0700, settings.json has mode 0600

### Coordination point
After Stream A finishes, it exports `SidecarHandle` for Stream B to consume. Shape must be agreed up-front:

```rust
pub struct SidecarHandle {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl SidecarHandle {
    pub fn request_stop(&self);  // sets stop flag
    pub fn is_finished(&self) -> bool;
    pub fn join(self) -> std::thread::Result<()>;  // consumes
}
```

## Stream B — `manager.rs` + `types.rs` cleanup & dispatch

**Delegate:** subagent working in the same checkout (serialized with Stream A — this stream starts *after* Stream A lands its coordination contract, but the actual file edits don't overlap).

### Tasks

1. **C1 (reap_exited cleanup):** In `SessionManager::reap_exited`, after the transition loop, call `session.cleanup_hook_artifacts()` on each transitioned session. Restructure borrow so both the status update and the cleanup happen in a single pass.

2. **C3 (drain-before-drop, types.rs side):** Rewrite `Session::cleanup_hook_artifacts` to:
   - Drain `hook_rx` into a local `Vec` of pending signals.
   - Call `sidecar_handle.request_stop()` to set the atomic flag.
   - Best-effort write-poke the FIFO to unblock a pending `File::open`.
   - Call `sidecar_handle.join()` with a **mandatory** timeout — if we miss it, log-error (not warn) with context.
   - Then drop `hook_rx`.
   - Return the drained signals so `reap_exited` can push them through `check_hook_signals` before reporting the exit.

3. **H5 (single-pass check_hook_signals):** Rewrite `SessionManager::check_hook_signals` using index-based iteration to drain + process in one pass without the "find by id" second loop.

4. **M1 (test seam invariant):** Update `install_test_hook_channel` to set both `hook_rx = Some(...)` AND a dummy `hook_dir = Some(...)` so the doc claim on `hook_rx` holds.

5. **M7 (cfg gate, types.rs side):** Gate the entire `install_hook` branch in `Session::spawn` with `#[cfg(unix)]`; on non-unix return an error "hook-based boundary detection requires Unix".

### Tests added in Stream B
- **T1** `reap_exited` cleans up hook artifacts (spawn dummy with install_test_hook_channel, mark exited, reap, assert cleanup ran — this is a unit-level check, not a real-PTY test)
- **T2** `Session::spawn` cleans up hook dir when `spawn_command` fails (inject a bad command, verify no `/tmp/ccom-*` dir exists after the error)
- **T3** concurrent two-session isolation using real FIFOs: spawn two hook-enabled sessions via `Session::spawn` with `/bin/cat` (doesn't need Claude), write distinct JSON to each FIFO by hand, verify signals route correctly
- **T4** end-to-end `check_hook_signals` with a real FIFO (skip test seam, use `Session::spawn` for a dummy)
- **T5** `cleanup_hook_artifacts` idempotency — call twice, second call is a no-op

## Stream C — final pass (serialized after A & B)

1. Run `cargo fmt && cargo clippy && cargo test`
2. Update `docs/pr-review-pr13.md` to mark each issue with its resolution (like PR #9's review doc)
3. Update `docs/designs/response-boundary-detection.md` §10 with any deltas from the fixes (e.g., the `SidecarHandle` addition)
4. Manual smoke test (the user will run this) — spawn a real Claude session, verify `ResponseComplete` fires end-to-end
5. Push and let the PR update

## Execution order

```
parallel:
  Stream A subagent (hook.rs)  ─┐
  Stream B subagent (manager + types)  ─┤
                                         ├→ Stream C
                                         │
                                         └→ push + re-review
```

In practice we'll launch A and B as two subagents in one message. B depends on A's `SidecarHandle` type existing, so we'll (1) prime the coordination contract by adding the type stub to `hook.rs` first, then (2) launch both subagents simultaneously — A finishes the real implementation, B consumes the stub.

## Verification

After both streams merge:
- `cargo test` — expect ~282+ tests passing (9 new tests added)
- `cargo clippy` — zero warnings
- `cargo fmt --check` — clean
- Manual: `cargo run`, spawn Claude, send prompt, see `ResponseComplete` in logs (RUST_LOG=debug)
- Manual: kill session, verify `/tmp/ccom-*` is cleaned up
- Manual: exit session naturally (Claude /exit), verify `/tmp/ccom-*` is *also* cleaned up (this was C1)
- Manual: spawn two Claude sessions, verify isolation
