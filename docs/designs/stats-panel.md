# Design: Stats Panel

**Status:** Parked — design complete, not yet scheduled for build.

## Motivation

The owner of ccom (and likely a meaningful chunk of its target users) has a
real, recurring decision they can't currently answer with the data Claude
Code exposes:

> "I rarely use my full Max quota per month, but I regularly capped out Pro
> at $20/mo. Am I on the right tier?"

The break-even tracker reframes this from a neat curiosity into a practical
right-sizing tool. The interesting math is not just "did API-equivalent spend
exceed the sub price this month," because **API credits roll over**. So:

- If you pay Pro ($20) and burn an extra $75 of API credits in a heavy
  month, you spent $95 total — **better than $100/mo Max**.
- If the next month you only use $20 of credits, unused credits carry forward.
  You lose nothing by staying on Pro.
- The break-even only tips toward Max when your *sustained average* of
  `Pro + API top-up > $100/mo`, accounting for the fact that credits don't
  expire month-to-month.

That rollover behavior matters. A naive "spend this month vs sub price"
comparison will steer users wrong. The panel should either show a rolling
average (say, trailing 90 days) or explicitly model the credits-roll case.

This is the real "wow" the feature needs to deliver: **tell me which tier I
should actually be on, given my real usage pattern, not a single month's
spike.** The raw today/per-session/token numbers are supporting cast.

### Two signals, two questions

The panel should combine two different views of the data because they answer
different questions:

1. **Rolling trailing-N average** (e.g. 90 days) — answers *"what is my
   actual steady-state usage?"* Smooths out spikes and quiet weeks. This is
   the primary driver of the tier recommendation: "on your trailing 90-day
   average, you're at $X/mo effective, stay on Pro."

2. **Week-over-week trend / projection** — answers *"where am I heading?"*
   Catches cases where usage is climbing or falling sharply and the rolling
   average hasn't caught up yet. Surfaces as a secondary nudge: *"but your
   last 2 weeks are up 40% — if that holds, Max pays off in ~6 weeks."*

A user who is stable on Pro shouldn't get pestered. A user whose usage is
clearly trending upward should get a heads-up before they spend three months
burning credits they could have avoided with Max. Vice versa for a Max user
whose usage has dropped.

## Goal

A new "Stats" panel in ccom showing:

1. **Total cost spent today** (across all sessions on this host).
2. **Per-session costs**, with the currently-attached session highlighted.
3. **Total token usage** broken down by input / output / cached.
4. **Break-even tracker**: "I pay $X/month for a sub — at what point does my
   API-equivalent spend exceed that, meaning the sub has paid for itself?"
5. **Day-over-day trend** (sparkline / small chart).

A footnote on the panel notes: *single host only — no cross-host aggregation.*

## Key constraint discovered during design

Claude Code's `cost.total_cost_usd` (exposed via the statusline payload) is a
**per-session running total**, computed client-side as `tokens × model pricing`.
It is **not** persisted in the transcript (`.jsonl`) — only the `usage` block
(token counts) and `model` are. Verified empirically: grepping assistant
records in a transcript shows `usage` but no `cost_usd` field.

Implications:
- For the **currently-active** session we can read cost directly from the
  statusline hook's output. Authoritative and free.
- For any **finished** session whose cost wasn't captured live, we cannot
  recover the cost without re-computing it from `usage × pricing_table`.
- Therefore, any "historical reconstruction" path requires maintaining a
  pricing table. Any "forward-looking tracking" path does not.

## Two approaches considered

### A. Parse transcripts + pricing table

Walk `~/.claude/projects/**/*.jsonl` incrementally, extract `usage` + `model`
per assistant message, multiply by a bundled per-model pricing table
(e.g. LiteLLM's `model_prices_and_context_window_backup.json`), store per
`(session_id, day, model)` in SQLite.

| Pros | Cons |
|---|---|
| Complete historical data from day one | Pricing table rots when Anthropic changes prices |
| No dependency on ccom being running | Approximation of Claude Code's number, not the real thing |
| Cross-midnight splits naturally | More code: ingester, pricing, upsert logic |
| Offline-safe | Pricing updates need a release unless we add remote refresh |

### B. Capture costs live via enhanced statusline hook *(preferred)*

Modify `scripts/ccom-statusline.sh` to write **per-session** files
(`~/.claude/ccom-sessions/{session_id}.json` containing
`{cost_usd, tokens, last_update}`) instead of (or in addition to) the single
global `ccom-rate-limits.json`. The statusline stdin payload already includes
`session_id`, so this is a small change.

ccom watches that directory. On each file change it applies a **delta-based**
update (see below) against a SQLite table.

| Pros | Cons |
|---|---|
| No pricing table — uses Claude Code's own authoritative number | No history for sessions that finished before ccom had this feature |
| Matches what Claude Code reports, exactly | Offline-gap problem (see below) |
| Much less code | Requires the statusline hook (already required for the existing quota panel — no new setup burden) |
| Per-session-per-day storage, tiny footprint | Can't recover from an uninstalled hook |

**Decision:** approach B. The "no historical reconstruction" tradeoff is
acceptable because the feature's value is forward-looking ("am I getting my
money's worth from the sub going forward"), not archaeological.

## Delta-based storage model (chosen)

Rather than snapshotting session cost at midnight boundaries, **record deltas
as they are observed** and attribute each delta to the day it was observed.

### Schema

```sql
CREATE TABLE session_day_cost (
  session_id   TEXT,
  day          DATE,
  cost_delta   REAL,      -- sum of deltas observed on this day
  tokens_delta INTEGER,
  PRIMARY KEY (session_id, day)
);

CREATE TABLE session_state (
  session_id        TEXT PRIMARY KEY,
  last_known_cost   REAL,
  last_known_tokens INTEGER,
  last_seen         TIMESTAMP
);
```

### Update loop

On each statusline update for session S with observed_cost = C:

```
delta = C - session_state[S].last_known_cost
if delta < 0:
    # cost went backwards → treat as session reset/resume; rebase
    last_known_cost = C
else:
    UPSERT session_day_cost(S, today) SET
        cost_delta = cost_delta + delta,
        tokens_delta = tokens_delta + <new_tokens - last_known_tokens>
    last_known_cost = C
    last_known_tokens = <new_tokens>
    last_seen = now
```

### Why deltas, not snapshots

Cost only accrues when a message is sent, and the statusline fires
immediately after. So "time we observed the change" ≈ "time it actually
accrued." We don't need a midnight snapshot tick, and the boundary case
handles itself.

### Edge cases covered

| Case | Handling |
|---|---|
| 5-day continuously running session | Each day's messages hit the statusline on that day; each day's deltas accumulate into their own row. No special logic. |
| Resume (same session_id, cost counter continues) | Identical to normal operation. Deltas keep accumulating. |
| Resume (same session_id, cost resets to 0) | `delta < 0` detected; rebase `last_known_cost = 0` and start fresh. No double-counting of past days. |
| Resume creates a new session_id | Simply a new session; clean. |
| Session spans midnight | The post-midnight statusline tick falls on the new day, so the delta is attributed to the new day. One message of inaccuracy at most. |
| ccom crashes and restarts | Picks up from `last_known_cost` — no loss. |
| Idle session | Statusline doesn't fire, `last_known_cost` stays put, `cost_delta` stays put. Correct: no tokens sent, no cost accrued. |

## Historical backfill via `stats-cache.json`

The live-capture approach has a cold-start problem: until ccom is running
with this feature, no data. The rolling average and trend signals are
useless for the first 90 days. **But Claude Code itself keeps a cache we
can read.**

`~/.claude/stats-cache.json` (version 3 as of this writing) is the source
file Claude Code's `/usage` command reads. Structure:

```json
{
  "version": 3,
  "lastComputedDate": "2026-04-09",
  "firstSessionDate": "...",
  "dailyActivity": [{"date": "2026-01-03", "messageCount": 63, "sessionCount": 2, "toolCallCount": 19}, ...],
  "dailyModelTokens": [{"date": "2026-01-03", "tokensByModel": {"claude-opus-4-5-20251101": 20912}}, ...],
  "modelUsage": {
    "claude-opus-4-6": {
      "inputTokens": 299871,
      "outputTokens": 2136881,
      "cacheReadInputTokens": 804685445,
      "cacheCreationInputTokens": 28020286,
      "costUSD": 0,
      "maxOutputTokens": 0
    },
    ...
  },
  "totalSessions": ...,
  "totalMessages": ...,
  "hourCounts": [...]
}
```

**Crucial observation**: every `costUSD` field in this cache is `0`. Claude
Code tracks **tokens** historically but does **not** persist cost. This
strongly suggests `/usage` displays token counts and quota percentages, not
dollar figures. (Verify empirically before building.)

### What this gives us

- **Token history for free**, going back to `firstSessionDate`, at daily
  granularity, broken out by model. No transcript parsing needed.
- **No pricing-rot risk for tokens** — this data is authoritative and
  owned by Claude Code.

### What it still can't give us

- **Historical cost** — we'd have to compute it ourselves by multiplying
  historical tokens × a pricing table. This re-introduces the rot problem,
  bounded to the backfill pass.

### Three options for cost history

1. **Tokens-only history, cost only for live-capture period.** Most honest.
   The panel shows "you've used X tokens over the last 90 days" and "since
   ccom started tracking on day T, your cost has been $Y." Tier recommendation
   uses live-capture data only, which means the recommendation takes ~2 weeks
   to get trustworthy. **Simplest and most accurate.**

2. **Estimated historical cost via bundled pricing table.** Multiply
   `dailyModelTokens` × a pricing table (LiteLLM snapshot) at backfill time.
   Label everything before the live-capture start as "estimated." Tier
   recommendation works from day one but may be inaccurate for periods when
   Anthropic's prices differed from the bundled table. Needs per-model
   price-history awareness to be fully correct — probably more trouble than
   it's worth.

3. **Hybrid**: use tokens-only history to compute a **tokens-per-day
   trend**, and combine with the live-capture period's **tokens-to-dollars
   conversion rate** to project cost. This finesses the pricing-history
   problem: we assume current-month cost-per-token is stable, and apply it
   uniformly to historical token volumes. Works as long as model mix and
   pricing haven't shifted dramatically. **Probably the right middle ground.**

### Recommendation

Go with **option 3 (hybrid)**. Pseudocode:

```
live_period_tokens = sum of tokens captured via statusline since ccom started
live_period_cost   = sum of cost deltas captured via statusline
cost_per_token     = live_period_cost / live_period_tokens   # effective rate

for each day D in stats-cache.json.dailyActivity:
    day_tokens_D = sum of tokensByModel for D
    estimated_cost_D = day_tokens_D × cost_per_token
```

Once the live period is long enough (say, 2 weeks of data), the effective
`cost_per_token` is accurate for this user's typical model mix. Applying it
backward gives a defensible historical cost estimate without maintaining a
pricing table at all.

**Edge case**: if the user has had major model mix changes (e.g. recently
switched from Sonnet to Opus), the blanket rate is wrong. Mitigation: do the
effective-rate calc **per-model**, then apply to historical
`dailyModelTokens` which are also per-model. Same idea, finer grain.

### Schema addition

```sql
CREATE TABLE backfill_daily (
  day              DATE PRIMARY KEY,
  tokens_input     INTEGER,
  tokens_output    INTEGER,
  tokens_cache_r   INTEGER,
  tokens_cache_w   INTEGER,
  est_cost_usd     REAL,     -- computed from live-period effective rate
  source           TEXT      -- 'stats-cache' | 'live'
);
```

Rebuild this on startup from `stats-cache.json`, overwritten each time the
effective rate changes. Live-period data continues to flow into
`session_day_cost` as designed; the UI merges both sources.

### Known inaccuracy: offline gap

If ccom (or the statusline hook) is not running for 2 days while a long
session keeps accruing cost, when observation resumes the big delta lands
entirely on the day of observation, not distributed across the offline days.

**Unfixable without the per-message data (approach A).** Accepted as a
tradeoff. In practice, users rarely run sessions without ccom if they care
about the stats panel.

## Open question before building: does Claude Code's resume reset cost?

I could not determine from the design alone whether Claude Code's `--resume`
flow:

1. Keeps the same `session_id` and continues the cost counter, OR
2. Keeps the same `session_id` but resets cost to 0, OR
3. Mints a new `session_id`.

The delta-rebase logic above is designed to handle all three defensively.
Before shipping, verify empirically by resuming a session and inspecting the
db state.

## Not-yet-designed pieces

- **Sub price UI**: "select" control on the stats page. Options: `$20 Pro`,
  `$100 Max 5×`, `$200 Max 20×`, `Custom…`. Persisted to
  `~/.config/ccom/config.toml`. Default `$100`.
- **Break-even semantics**: month-to-date by default. Billing-cycle alignment
  would need the user's cycle start day — not worth it for v1.
- **Keybind**: `Alt+T` for "sTats" (Alt+U = usage, Alt+S = session picker,
  Alt+D = dashboard, Alt+M = mouse, Alt+O = back-to-dashboard).
- **Pricing table refresh** (only relevant if we pivot to approach A):
  bundle LiteLLM snapshot, lazy refresh from GitHub with 7-day TTL, graceful
  offline fallback.
- **Watcher implementation**: `notify` crate for inotify/kqueue, with a 1s
  poll fallback for platforms where it's flaky.
- **SQLite crate**: `rusqlite` — ccom is synchronous and we don't need the
  async baggage of `sqlx`.

## Implementation checklist (when we come back to this)

1. Update `scripts/ccom-statusline.sh` to write per-session files to
   `~/.claude/ccom-sessions/{session_id}.json`. Keep the global
   `ccom-rate-limits.json` write for backward compat with the existing
   quota panel.
2. Add `rusqlite` dependency. Create `src/stats/mod.rs` with the schema
   above and a `Stats` struct exposing query methods (today_total,
   per_session_today, day_over_day, etc.).
3. File-watcher loop (runs on the existing app tick) that reads each
   `ccom-sessions/*.json` and applies the delta update.
4. `src/ui/panels/stats.rs` rendering the panel.
5. Wire up `Alt+T` keybind and panel dispatch in `src/app.rs`.
6. Sub price config: add to the existing config file, render a select widget.
7. Break-even math and display.
8. Footnote: `*stats reflect sessions on this host only`.

## References

- `src/claude/rate_limit.rs` — existing statusline-file reader
- `scripts/ccom-statusline.sh` — existing statusline hook to be extended
- `src/ui/panels/usage_graph.rs` — existing quota panel (pattern to follow)
- https://github.com/BerriAI/litellm/blob/main/litellm/model_prices_and_context_window_backup.json — pricing table reference (only needed for approach A)
