# Phase 7 Task 11 — End-to-end manual verification

**Goal:** confirm the full approval-routing stack works in a real session, and answer the key question: does the driver receive approval requests via push (subscribe notification) or does it need to poll?

## Setup

```bash
git pull origin main
cargo build --release
RUST_LOG=ccom=debug cargo run
```

Start ccom normally, then use the TUI to spawn a driver session (role: Driver, spawn policy: budget, budget: 3).

---

## Scenario 1 — Happy path: allow once, then allow always

1. In the driver's chat, send:
   > "Spawn a helper in `<path to this repo>`. Have it run `ls -la src/` and report the output."

2. **Watch for:**
   - Driver spawns child (Phase 6 happy path — no modal)
   - Child attempts `Bash: ls -la src/`
   - The `▲` marker appears next to the driver in the session list
   - The driver's status line shows `▲ 1 pending approval(s)`

3. **Key observation — record the answer:**
   - Did the driver's LLM receive the approval request **automatically** (push via `subscribe` notification)?
   - Or did nothing happen and the driver had to call `list_pending_approvals` to discover it?
   - **This determines whether Task 7 is needed.**

4. Respond to the driver (or have it decide): approve with `scope: "allow_always"` via `respond_to_tool_approval`.

5. Child runs `ls -la src/` and proceeds.

6. Ask the child to run `ls -la docs/` next. The hook should **short-circuit via the state file** — no daemon round-trip, no approval request.

7. **Verify the state file:**
   ```bash
   cat "$XDG_STATE_HOME/ccom/sessions/<uuid>/approvals.json"
   # or if XDG_STATE_HOME is unset:
   cat "$HOME/.local/state/ccom/sessions/<uuid>/approvals.json"
   ```
   Should contain the `allow_always` rule for `Bash`.

8. **Verify the `▲` marker clears** after the approval resolves.

---

## Scenario 2 — Driver exits mid-approval

1. Spawn a new driver + child pair.
2. Child triggers a tool that needs approval.
3. **Before approving**, kill the driver session from the TUI.
4. **Verify within a few seconds:** the child's pending hook request resolves to `deny` (child gets `{"decision":"deny","reason":"ccom: driver exit"}`).
5. The `▲` marker should disappear (or the driver row should disappear entirely).

---

## Scenario 3 — Solo session pass-through intact

1. Create a Solo session (no driver, default spawn).
2. Have it run any `Bash` command.
3. **Verify:** the TUI modal fires as before (allow-once / deny / always). No approval routing involved — hook exits immediately with the user's choice.
4. **Check timing:** look at debug logs for `ccom_hook_timing` — solo session pass-through should be sub-millisecond.

---

## Pass/fail criteria

| Check | Expected |
|---|---|
| `▲` appears on driver when child awaits approval | Pass |
| Push notification reaches driver LLM without polling | Pass = skip Task 7; Fail = implement Task 7 |
| `allow_always` persists in state file | Pass |
| Second tool call short-circuits without approval request | Pass |
| Driver exit resolves pending to Deny in <5s | Pass |
| Solo session TUI modal fires normally | Pass |
| Solo hook timing < 1ms (debug log) | Pass |

---

## After Task 11

- If push notification **works**: Task 7 is skipped, Phase 7 is complete.
- If push notification **does not work**: implement Task 7 (`list_pending_approvals` polling tool), then re-verify.
