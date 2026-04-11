# Model Council — Design Spec

Status: draft
Author: @mkrueger
Date: 2026-04-11

> **Prerequisite:** [`docs/designs/session-management.md`](designs/session-management.md).
> The Council consumes the event bus, response boundary detector, and
> programmatic write path defined there. Land session-management
> Phases 1–3 before the Council's Phase 2. The bundled MCP server and
> driver-session role (session-management Phases 4–6) are independent
> of the Council and are *not* required to ship it.

## 1. Motivation

Perplexity shipped "Model Council" in late 2025: a single prompt is fanned out
to three frontier models in parallel (GPT-5.2, Claude Opus 4.6, Gemini 3 Pro),
and a fourth "synthesizer" model reconciles the three completions into one
answer that highlights where the models agree, where they diverge, and what's
unique to each. Disagreements are surfaced rather than voted away — the pitch
is "when models converge, move faster; when they disagree, dig deeper."

Claude Commander already manages N parallel Claude Code sessions in a TUI. The
primitives we need for a Council feature — spawning multiple sessions,
routing a prompt, capturing output — are already in place. What's missing is
model pinning per session, a broadcast dispatch path, output extraction, and
a synthesizer role.

This doc specs the feature at a high level. It does not prescribe
implementation details beyond naming the integration points.

## 2. User story

1. User opens a Council from the command bar (e.g. `/council` or a keybind).
2. A modal asks which models to include. Defaults to a 3-model preset the
   user configured up front (e.g. Opus 4.6, Sonnet 4.6, Haiku 4.5). User can
   save presets with names ("all-anthropic", "frontier-mix", etc.).
3. Commander spawns N+1 sessions in one batch: one Claude Code session per
   panelist model, plus one synthesizer session. All share the current
   working directory.
4. User types a single prompt into the Council input. The prompt is
   broadcast verbatim to each panelist session.
5. Commander watches each panelist session for completion, scrapes its
   response, and streams a live "N of M responded" status in the UI.
6. Once all panelists are done (or a timeout/cancel fires), Commander
   assembles a synthesis prompt and sends it to the synthesizer session.
7. The synthesizer's output is the primary answer shown to the user. Each
   panelist's raw response remains inspectable by jumping into its session
   like any other Commander session.

Non-goals for v1:
- Voting / majority selection (we surface disagreement, we don't resolve it).
- Tool-use coordination across panelists (each session runs independently).
- Cross-provider models. Anthropic-only to start; the design leaves room.

## 3. What we have to work with

All file/line references are against `main` at 2d69c01.

### 3.1 Session model

- `Session` struct — `src/session/types.rs:25`. Owns the PTY, a vt100 parser
  (1000-line scrollback), status enum, working dir.
- `SessionManager` — `src/session/manager.rs:42`. Vec-backed, ids are
  monotonically increasing and never reused. Already exposes `iter_mut()`.
- `SpawnConfig` — `src/session/manager.rs:32`. No model field today.
- Session spawn is driven by `spawn_session_kind()` in `src/app.rs:949`,
  which builds args via `launcher::claude_args()` (currently returns an
  empty vec). Extra flags come from the new-session modal and are
  whitespace-split.

### 3.2 Input dispatch

- Per-keystroke writes land in `Session::try_write()` at
  `src/session/types.rs:151`, which writes raw bytes to the PTY master.
- The only entry today is `App::handle_session_view_key()` at
  `src/app.rs:669`, i.e. the active session. No broadcast helper exists.

### 3.3 Output capture

- A dedicated reader thread per session pipes PTY bytes through
  `Event::PtyOutput { session_id, data }` (`src/session/types.rs:77`) and
  into the vt100 parser. There is no persistent transcript — output lives
  in the parser's screen buffer.
- `PromptDetector` at `src/pty/detector.rs` already pattern-matches Claude
  Code's interactive prompts ("Allow once", "Y/n", etc.). This is the
  natural place to add a "response complete" detector.

### 3.4 UI

- Dashboard layout splits file tree / session list / usage graph
  (`src/ui/layout.rs:10`).
- Full-screen SessionView mode renders the vt100 buffer for one session
  (`src/ui/panels/session_view.rs`).
- SessionPicker overlay already demonstrates the pattern for a modal that
  enumerates sessions (`src/app.rs:677`).

### 3.5 Claude Code model pinning

Claude Code supports `--model <id>` for pinning, accepting aliases
(`sonnet`, `opus`, `haiku`) or full IDs
(`claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`).
That's the hook we need — no custom API integration required.

## 4. Architecture

### 4.1 New data

- `Session.role: SessionRole` — `Solo | CouncilPanelist { council_id } |
  CouncilSynthesizer { council_id }`. Default `Solo`. Solo sessions behave
  exactly like today.
- `Session.model: Option<String>` — the resolved `--model` value, displayed
  in the session list.
- `SpawnConfig.model: Option<String>` and `SpawnConfig.role: SessionRole`.
- A new `Council` aggregate owned by `App`:
  ```
  struct Council {
      id: CouncilId,
      panelists: Vec<SessionId>,
      synthesizer: SessionId,
      pending_prompt: Option<String>,
      responses: HashMap<SessionId, CouncilResponse>,
      state: CouncilState, // Idle | Broadcasting | Collecting | Synthesizing | Done
  }
  ```
- A `CouncilPresets` config file at `~/.config/claude-commander/councils.toml`
  listing named presets (name + ordered list of models + optional
  synthesizer model, defaulting to Opus).

### 4.2 Spawn path

`App::spawn_council(preset)` in `src/app.rs` near `spawn_session()`
(`src/app.rs:936`):

1. For each panelist model, build a `SpawnConfig` with
   `args = ["--model", model]` plus whatever the user's default flags are,
   and `role = CouncilPanelist { council_id }`.
2. Spawn the synthesizer session the same way with its own model.
3. Register the `Council` in `App.councils` keyed by `council_id`.
4. Label sessions clearly: `council-{id}/opus`, `council-{id}/sonnet`,
   `council-{id}/haiku`, `council-{id}/synth`.

### 4.3 Prompt broadcast

New method `SessionManager::broadcast(ids: &[SessionId], bytes: &[u8])` that
loops calling `Session::try_write()`. The council controller calls it with
the panelist ids (never the synthesizer) when the user submits a prompt.

A new `AppMode::CouncilInput { council_id }` wires the command bar to this
path so the user can type into the Council without being in any single
session.

### 4.4 Response extraction

This is the hardest part because Claude Code output in the PTY is a
terminal stream, not structured data. Two options:

**Option A — scrape the vt100 buffer.** Extend `PromptDetector` (or add a
sibling `ResponseBoundaryDetector`) that watches for the "idle prompt
returned" state. When a panelist session transitions from "working" to
"idle" with new content since the last prompt submission, treat the delta
between the two idle markers as the response body. Requires tracking a
"last prompt submitted at line N" marker per session and normalizing ANSI.

**Option B — run panelists with `claude -p` (print / non-interactive
mode).** In `-p`, Claude Code emits the response to stdout and exits. No
TUI chrome, no scraping, much cleaner boundaries. Downside: the session
isn't "alive" afterward, so the user can't jump in and continue it — which
breaks the UX goal of inspecting each panelist's reasoning in its full
session.

**Decision: Option A (interactive PTY scraping).** Rejecting the hybrid
and rejecting pure `-p` for two reasons:

1. **Generality across runners.** The long-term goal is for a panelist to
   be any interactive CLI agent — Claude Code today, but potentially
   Gemini CLI, OpenCode, Aider, etc. `claude -p` is a Claude-Code-specific
   escape hatch; interactive PTY scraping is the lowest common
   denominator that works for any agent that has a REPL. Building the
   extraction layer against the PTY keeps the council runner-agnostic.
2. **Follow-ups.** A live interactive session lets the user (or the
   synthesizer, in a future iteration) submit a follow-up prompt to any
   panelist without re-spawning it and losing conversation state. A `-p`
   invocation is one-shot and throws that context away.

Accept that response extraction will sometimes be fuzzy around tool-use
chatter, and invest in `ResponseBoundaryDetector` accordingly. Revisit if
and when a structured output stream becomes available across the agents
we care about.

### 4.5 Synthesis

Once all panelists have reported (or a per-panelist timeout fires, missing
panelists flagged as "no response"), the council controller:

1. Assembles a synthesis payload (see §5 for the prompt).
2. Writes it to the synthesizer session via `try_write()`.
3. Watches the synthesizer the same way panelists are watched and streams
   its output into the Council view.

### 4.6 UI surface

- A new `CouncilView` panel (sibling of `SessionView`) rendering: top =
  user prompt + status strip ("opus ✓  sonnet ✓  haiku …"), middle =
  synthesizer output stream, bottom = tab strip to jump into any
  panelist's full session.
- Session list (`src/ui/panels/session_list.rs`) gets a "Council" column
  or groups council members visually under a parent row.
- Command bar gains `/council <preset>` and a keybind (e.g. `Alt+C`) to
  open the preset picker.

### 4.7 Lifecycle

- Killing the council kills all N+1 sessions together. `SessionManager`
  already has exit handling (`reap_exited` at `manager.rs:245`); the
  controller listens for `SessionExited` events on any council member and
  decides whether to tear down the group or mark a panelist as dead.
- If one panelist crashes mid-broadcast, synthesis proceeds with the
  survivors and the missing one is noted in the synthesizer prompt.

## 5. Synthesizer prompt (v1 draft)

Stored at `~/.config/claude-commander/council-synthesis.md`, user-editable,
with the following default body interpolated at send time:

```
You are the synthesizer for a Model Council. Three Claude models were
given the same user prompt independently and produced the responses
below. Your job is to produce ONE unified answer for the user.

Rules:
1. Do not pick a "winner." Where the panelists agree, state the consensus
   confidently. Where they disagree, surface the disagreement explicitly
   and explain what's at stake in the difference.
2. Attribute specific claims to specific panelists when the attribution
   matters (e.g. "Opus suggests X, Sonnet and Haiku prefer Y because…").
3. If a panelist is missing a response, note it and proceed with the
   others. Do not speculate about what the missing model would have said.
4. Prefer a short synthesis over a long one. If the panelists all agree,
   the synthesis should be nearly as short as one of their answers.
5. End with a "Confidence" line: High / Medium / Low, based on panelist
   agreement and your own read of the answer.

User prompt:
<<<
{user_prompt}
>>>

Panelist responses:

## {panelist_1_model}
<<<
{panelist_1_response}
>>>

## {panelist_2_model}
<<<
{panelist_2_response}
>>>

## {panelist_3_model}
<<<
{panelist_3_response}
>>>
```

Open questions on the prompt:
- Should the synthesizer see the panelists' tool-use traces or only their
  final prose? v1: final prose only. Traces are noisy and token-heavy.
- Should the synthesizer be told which model it is itself? Probably yes
  (helps it calibrate self-referential disagreement).

## 6. Open questions

1. **Config surface.** TOML preset file vs. in-TUI preset editor. Suggest
   TOML for v1, editor later.
2. **Output extraction robustness.** PTY scraping is the chosen path
   (§4.4) — needs a spike to validate `ResponseBoundaryDetector` handles
   tool-use chatter, streaming markdown, and agent-specific idle markers
   across at least Claude Code and one other runner.
3. **Synthesizer model default.** Opus for quality, Sonnet for speed.
   Suggest Opus as default, preset-configurable.
4. **Panelist parallelism.** All three broadcast simultaneously — fine for
   API-backed Claude Code, but if any panelist is rate-limited the whole
   council stalls. Needs per-session timeout handling.
5. **Cost visibility.** A Council is ~4x a normal prompt. The usage graph
   panel (`src/ui/panels/usage.rs`) should annotate council runs so the
   user can see the multiplier.
6. **Cross-provider future.** Keeping `model: Option<String>` as a free
   string (rather than an enum) leaves the door open to future non-Claude
   panelists, assuming we ever front a non-Claude-Code runner.

## 7. Phased plan

**Prerequisite:** `docs/designs/session-management.md` Phases 1–3
(event bus, response boundary detector, programmatic write path). The
Council consumes all three and doesn't have a clean landing path
without them.

- **Phase 1:** per-session model pinning (`--model` plumbing through
  SpawnConfig, displayed in session list). Independent of the prereq;
  useful on its own and can land in parallel.
- **Phase 2:** council data model + spawn helper. Uses
  `SessionManager::broadcast()` from the session-management spec for
  the broadcast writer. No synthesizer yet — "send this to all three,
  let me tab between them manually."
- **Phase 3:** synthesizer wiring. Uses `ResponseComplete` events from
  the session-management bus to know when all panelists are done.
- **Phase 4:** CouncilView UI polish, presets, usage graph annotation.

Each phase lands behind the others without breaking solo-session usage.

## References

- Perplexity Model Council launch: https://www.perplexity.ai/hub/blog/introducing-model-council
- Perplexity help center: https://www.perplexity.ai/help-center/en/articles/13641704-what-is-model-council
- Third-party writeup (architecture angle): https://medium.com/design-bootcamp/perplexity-model-council-multi-model-consensus-as-an-ai-verification-architecture-eedb14603e19
