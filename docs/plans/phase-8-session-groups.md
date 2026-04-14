# Phase 8 — Session Group Restoration

**Branch:** `docs/phase-8-session-groups` (for this capture doc; implementation branch TBD)
**Depends on:**
- Claude session UUID capture (parallel work stream — populates `Session::claude_session_id` from the Stop hook JSON)
- Phase 6 — Driver Role (for role + attachment_map serialization)
- Optionally Phase 7 — Approval Routing (see open question 11)

**Blocks:** Nothing.

**Status:** **IDEA CAPTURED, NOT YET PLANNED.**

This document is a parking spot for the feature shape so the idea isn't lost. It is deliberately short. A real plan — with tasks, tests, and acceptance criteria in the style of `phase-6-driver-role.md` — happens later, when Phase 8 is actually kicked off and its prerequisites have landed.

---

## Why

**User goal:** "I'll restart previous sessions so I don't have to manually re-spawn the five sessions I was working with yesterday in this repo."

**Use case:** A user opens ccom in a repo, spawns several Claude sessions (maybe a driver plus a few children, maybe just a handful of solo sessions), does a day's work, and exits. The next morning they `cd` back into the repo, run `ccom`, and want yesterday's fleet to come back — same labels, same working dirs, same roles, same conversation history. No manual re-spawning, no copy-pasting of session UUIDs, no re-attaching children to the driver.

The Claude CLI already supports resuming a conversation via `claude --resume <uuid>`. The only thing ccom is missing is (a) persistent knowledge of which sessions were live, and (b) a restore step at startup that re-spawns each one with `--resume`.

---

## Rough shape

1. **Persist on exit.** When ccom exits cleanly, serialize the live session list to a state file. Each entry carries: `label`, `working_dir`, `claude_session_id` (UUID), `role` (Solo / Driver { policy, budget } / child-of-driver), `spawned_by` (parent driver's session id, if any), and any `attachment_map` entries keyed by this session.
2. **Persist opportunistically.** Flush on lifecycle events (spawn, kill, role change) so a crash doesn't lose the whole day's fleet. See open question 6.
3. **State file location.** Per-workdir: `<repo>/.ccom/state.json` (recommended — see open question 1). Gitignored by convention; ccom can add `.ccom/` to the repo's `.gitignore` on first write if desired, or leave that to the user.
4. **Offer restore on startup.** If ccom starts in a workdir that has a non-empty state file, prompt: `Restore N previous sessions from <timestamp>? [y/N]`. On `y`, iterate the serialized list and spawn each with `claude --resume <uuid>`, re-applying role and attachment mapping. See open question 4 for auto-restore.
5. **Degrade gracefully.** If `--resume` fails for a given UUID, skip that session, log the failure to the status line, and continue restoring the rest. See open question 3.
6. **Translate ids.** Restored sessions get fresh ccom-local ids. Serialized attachment maps are rewritten through the old-id-to-new-id translation. See open questions 8 and 9.

---

## Open design questions

Each has a **recommendation** but no final commit. These get decided when Phase 8 is actually picked up.

### 1. Scope of a "group" — per-working-directory or per-ccom-run?

**Recommendation:** per-workdir. Restart ccom in `/repo/foo` → restore the sessions that were in `/repo/foo`. Lower blast radius, more intuitive for the "I work in this repo daily" case, and the state file lives next to the project it belongs to. A per-run model would force a global state file and is harder to reason about when the user has multiple unrelated projects.

### 2. Role restoration

If the pre-exit state had a driver with three children, Phase 8 restores the driver **with the same role** (Driver, same `spawn_policy`, same `spawn_budget`) and the three children **with the same parent relationship**. The `attachment_map` also serializes. This is the whole point of the feature; skipping role restoration would make restored fleets functionally different from their originals.

**Recommendation:** yes, fully restore roles + attachment map. No half-measure.

### 3. Resume failure handling

`claude --resume <uuid>` can refuse for several reasons: transcript corrupted, UUID collision, Claude Code version mismatch, transcript file moved/deleted, disk relocated.

**Recommendation:** skip the failing session, restore the rest, and surface a per-session failure message in the status line (something like `restore: session 'api-work' failed — transcript not found, skipping`). All-or-nothing is too brittle.

### 4. Auto-restore vs. prompt on startup

**Recommendation:** prompt by default (`Restore N previous sessions? [y/N]`). Silent auto-restore is surprising the first time it happens. Offer an opt-out for power users via either:
- a `--auto-restore` CLI flag, or
- a `driver.toml`-style config entry (`auto_restore = true`).

Either way the prompt is the safe default.

### 5. Concurrent ccom instances

If two terminals run ccom on the same workdir, they'd both try to restore the same state file — double-spawning every session.

**Recommendation:** lockfile on the state file (`<repo>/.ccom/state.lock`) with a short timeout (say 2 seconds). The second instance sees the lock, skips restore entirely, and comes up with an empty fleet. Last-writer-wins on exit. This is good enough for a local tool; proper distributed coordination is out of scope.

### 6. State staleness — when do we flush?

**Recommendation:** clean-exit flush (authoritative) **plus** opportunistic flushes on session lifecycle events (spawn, kill, role change, attachment change). No timer. On a crash, the state reflects the most recent lifecycle event, which is close enough for day-to-day use.

### 7. Integration with the `--driver` CLI flag (Phase 6)

Phase 6's `--driver` flag is for **creating** a driver at startup. Phase 8 restoration brings a driver back from serialized state — its role just comes back, no flag needed.

**Recommendation:** document explicitly that `--driver` is unnecessary (and inert — or perhaps a no-op warning) when a restore happens. Users should not expect to re-pass their startup flags; the state file is authoritative for restored sessions.

### 8. Session identity across restarts

ccom session ids are per-run (`1`, `2`, `3`, ...). After restoration, should restored sessions keep their old ids or get fresh ones?

- Old ids preserve the "it's the same session" UX but risk collision if the user ALSO passes `--spawn N` on startup (fresh spawns would need ids disjoint from the restored range).
- Fresh ids sidestep collisions entirely.

**Recommendation:** **fresh ids on restore.** Keep the labels and working dirs identical so the user still recognizes each session. Print the old-id-to-new-id mapping once in the status line on restore (`restored: [2→1 'api', 5→2 'ui', 7→3 'driver']`), then carry on. Users care about labels more than numeric ids.

### 9. How does the attachment map survive?

Serialize `attachment_map` alongside the session list. On restore, rewrite both the keys (driver ids) and values (attached child ids) through the old-id-to-new-id translation from question 8. Straightforward bookkeeping — not a hard problem once (8) is decided.

### 10. PTY state does **not** survive

Scrollback, cursor position, in-flight editor state, TUI redraw state inside the child — all lost. The restored session starts with a fresh PTY. Claude Code's `--resume` will replay enough context on startup to fill the visible terminal with conversation history, but the pre-restart scrollback buffer is gone for good.

**Recommendation:** document this as an accepted limitation. Don't try to persist PTY framebuffers; that way lies madness.

### 11. Interaction with Phase 7 (approval routing)

If Phase 7 lands before or alongside Phase 8, restored driver children should still route approvals to their driver post-restart. This works automatically **if** Phase 7's hook installation is universal (per "Option E" in the Phase 7 planning discussion) — every Claude session gets the approval hook installed regardless of how it was spawned, so restored children inherit routing for free as soon as their role + parent relationship is restored.

**Recommendation:** confirm end-to-end once both phases are implemented. No Phase 8 work should be required here if Phase 7 is built correctly; this is a validation checkpoint, not a task.

---

## Prerequisites

- **Claude session UUID capture** (parallel work stream). `Session::claude_session_id` must be populated from the Stop hook JSON for every session Phase 8 expects to restore. Without this, there's nothing to hand to `--resume`.
- **Phase 6 — Driver Role.** Phase 8 serializes `SessionRole`, `SpawnPolicy`, `spawn_budget`, and the `attachment_map`. These types must exist and be stable before Phase 8 can commit to a state file schema.
- **(Optional) Phase 7 — Approval Routing.** Not a hard dependency, but see open question 11. If Phase 7 is in flight, validate the interaction before declaring Phase 8 done.

---

## Explicitly deferred (out of scope for Phase 8 v1)

- **Restoring sessions ccom didn't originally spawn.** If the user launched `claude` in a separate terminal and ccom never knew about it, ccom can't restore it. The state file only tracks ccom-managed sessions.
- **Restoring terminal sessions.** There's no `--resume` equivalent for bash or other shell sessions. If ccom ever supports non-Claude session types, they're ineligible for restoration.
- **Cross-machine state portability.** The state file is local. UUIDs reference transcripts on this machine's disk. Copying the state file to another box is not supported and not worth designing for.
- **Selective restore** ("bring back sessions 2 and 4 out of 5"). v1 is all-or-nothing on the restore prompt. A future iteration could offer a multi-select UI, but not now.
- **State file schema migration.** v1 assumes the same ccom version that wrote the file is the one reading it. A version-mismatched state file should be refused politely (with a clear error) rather than migrated. Proper migrations are a Phase 8.1 problem if they ever matter.
- **Sharing groups across workdirs.** A session spawned in `/repo/foo` is restored only when ccom is started in `/repo/foo`. No "global session list" view.

---

## Next steps

None until the UUID capture work stream lands. Once it does, revisit this doc, promote it to a full plan in the style of `phase-6-driver-role.md`, and break it into tasks.
