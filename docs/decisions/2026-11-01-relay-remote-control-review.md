# Review: Relay as ATOMICS's remote control (design d1)

Reviewed: `docs/decisions/2026-11-01-relay-remote-control.md` at `1747bdc` (called **d1** below; `d1:N` is its
line N). Reviewer: host session with every repository, read only, 2026-11-01. Nothing was built or run.

Outcome: every finding below is **resolved in design d2** (same file, revised in place). The last section,
[Resolved in d2](#resolved-in-d2), lists each change with its d2 section.

The review also read Relay's code and called its live tools. Its findings about Relay's side (where Relay's
capabilities live, its frontend and push, its notification paths, its development links and webhook
configuration, and Relay's own work items) are kept with Relay. This record keeps what concerns ATOMICS, and
summarizes a Relay-side finding only for what it changes in ATOMICS.

## Verdict

**Not ready to become implementation tasks.** The design's ATOMICS core holds up: a local authority rule,
pasted text only, Held during permission prompts, an outbox with per-write nonces, and redaction through the
trace `Cleaner`. But four things must change first:

1. The chief's Q2 answer (a ticket-linked personal conversation) replaces d1's comment threads. Relay's
   conversation had **no ticket link and no MCP surface**, and **required a personal agent**, while the user's Q1
   clarification lets the chief connect as a product agent, which can't own one. So Relay's side must generalize
   the conversation, and ATOMICS must reach it through generic operations; parts of X2/X4 must be rewritten (H1).
2. Push to the phone needs a build on Relay's side, not a check (H2).
3. ATOMICS's attention comments can't notify anyone. MCP comments carry no mentions, and where mentions do
   notify, the lock screen shows the comment text (H3).
4. The doc still describes the options the chief's answers rejected (Q3, Q4). Its Q1 owner list is right,
   but it lets the Human-kind check be switched off and expects Relay to add an owner to product agents. Its
   Q5 hope fails: ATOMICS branch names don't match Relay's development links (H4, H5).

What can start now: X1's remainder (whoami, comment identity paths, outbox) once the ticket-writes plan's a1 merges
(M5). Everything else waits for a d2 revision of the doc along the fixes below.

---

## 1. The chief's answers against the doc

| Q | Chief's answer | Doc must change |
| --- | --- | --- |
| Q1 | Chief's answer: "only the human owner of the OAuth personal agent ATOMICS signs in as". **Superseded by the user's clarification:** ATOMICS's own identity and the steering human are separate. The chief may connect as any agent; in the user's setup that is the product agent, which has broad, sysadmin-like permissions and no owner. The user's personal agent is separate: an OAuth personal agent. Steering and approval rights come from a configured **owner list of human user ids**. It defaults to whoami's `ownerUserId` when the connection is a personal agent, and must be set explicitly when it isn't. Comments from any agent identity never steer | d1's rule already matches: `d1:343-347` (owners if set, else whoami's owner, else off) and checks 2-4 of `d1:358-365`. Fix these lines: <br>• `d1:232`: say "**human** user ids", and make the empty-list default apply only when `whoami.kind` is a personal agent. <br>• `d1:233`: `require_kind = ""` must not exist; agent comments never steer, so only the value is configurable (M2). <br>• `d1:369-372`: signing in as the product agent with explicit `owners` is the intended setup; delete "(or Relay fills `owner_id`, G-R3)", because product agents have no owner by design. <br>• `d1:131-133`, `d1:444-445` and the Relay work item at `d1:666`: delete G-R3 (whoami already reports the owner of personal agents, and product agents should have none). <br>• `d1:334`: the chip "product-agent (agent of Alice)" is wrong → "relay · Product Agent (agent) · steering: Alice", or "relay · Alice's agent · steering: Alice (owner)". <br>• Doctor note at `d1:555-556`: with no owner list and no whoami owner, say "set `[tickets.remote] owners`". <br>• Close open question 1 `d1:746-748`; the product agent is not, and should not be, an owned agent |
| Q2 | A ticket-linked personal conversation, if the code supports a link | Replace `d1:434-442` and the Relay work item it planned with H1's plan. `d1:129-130` G-R2 becomes "a ticket-linked conversation for any agent connection, with an MCP surface". `d1:82` "Not a generic MCP surface" is right; keep it |
| Q3 | Owner only by default | `d1:277` ("anyone who can read the ticket…"), `d1:389-390`, `d1:441-442` ("the scope comment wants others to see the thread") → owner only. Remove `target = "comments"` (`d1:285-287`): it would publish the conversation to every ticket reader. Close open question 3 |
| Q4 | Steering and `/approve` from the Conversation only | Remove `steer = "comments"` and `steer_prefix` (`d1:234-235`, `d1:253`), Option A's "on the plan's own ticket" (`d1:206-216`), and Option B (`d1:218-222`, X3 step 3). Post the approval request into the Conversation, not as a main comment (`d1:195-201`). Close open question 4 |
| Q5 | (asked of this review) | No match; see H5. `d1:77`, `d1:327-329`, open question 5 |
| Q6 | Phone approval of permissions and merges is phase 2 | Already consistent (`d1:224-225`); close open question 6 |
| Q7 | No extra MFA for plan approval in v1; irreversible actions stay desktop-only | Approval → `approve_plan` → `plan` event; dispatch pushes nothing (`crates/atomics/src/app/plans.rs:266-273`). Accept for **approval**. Steering is broader than approval (M4): `d1:394-395` and §4.3 must list the desktop-only actions and add the chief rule. Close open question 7 |
| Q8 | Target Relay's current frontend | It has no PWA yet (H2): `d1:83`, `d1:84` and `d1:479-480` change on Relay's side. Close open question 8 |
| Q9 | Adaptive polling, 5 s active, 60 s otherwise | `d1:236-237` already says this, but a `get_ticket` poll is heavy (M9). Define "active" (M9); forbid the newest-N `get_ticket` fallback at 5 s (`d1:244-246`). Close open question 9 |

---

## 2. Findings, by severity

### High (block the implementation tasks)

**H1. The personal conversation (Q2) needs a ticket link and an MCP surface; d1 plans neither.**
- Relay's conversation machinery already covers most of the remote: owner-only send and read, queued and
  delivered acknowledgements, and attention pushes from activity states that map onto ATOMICS's "needs you"
  (permission prompt → waiting for approval, plan approval or question → waiting for input, blocked → failed).
- It lacked a ticket link, worked only for personal agents (so not for the product agent the chief connects as),
  and was reachable only through Relay's runner protocol. Speaking that protocol would put a Relay client into
  ATOMICS, which breaks agnosticism. Relay's side therefore adds a ticket-linked conversation that any agent
  connection opens, naming its **steering humans** (ATOMICS's owner list), readable and steerable only by them,
  and MCP tools to open it, post events, read steering after a cursor and acknowledge.
- An event summary is at most 2000 characters, not d1's 4000 (`d1:280`).
- **Fix (ATOMICS):**
  - generic operations: `conversation_open` (`{uuid}`, `{owners}`), `conversation_post` (`{nonce}`, `{role}`,
    `{body}`, `{activity}`), `conversation_read` (`{after}`) and `conversation_ack`;
  - the verdict function still requires author ∈ owners, kind Human and author ≠ `whoami.id`, as defence in
    depth;
  - drop `threadKey`/`role` on comments and `list_ticket_comments` (`d1:434-441`).

**H2. Push to the phone is a build on Relay's side.** d1 treats it as a check to confirm (`d1:84`, `d1:149-150`,
`d1:479-480`, `d1:702-703`). Nothing changes in ATOMICS, which never sends push itself.

**H3. Attention comments can't notify, and mention pushes leak text.**
- MCP comments pass no explicit mentions, so `@owner-handle` (`d1:200`), `notify_mention` (`d1:316-319`) and §5.1's
  mention-based rows (`d1:409-411`) never fire.
- Where mentions do fire (browser comments), the push shows part of the comment. That contradicts "lock screens
  never show conversation text" (`d1:387-388`).
- **Fix:** delete attention comments and `notify_mention`. Approval needed, needs you and blocked become
  activity states on the conversation (H1), which push generic copy; plan finished becomes a Completed
  event. CI failed and deployed notices are Relay's, with generic titles and no message body. If a mention path is
  ever added, the push body must be empty.

**H4. The doc still specifies what Q3 and Q4 rejected, and two Q1 details are wrong.** See the table in §1
for each line. The biggest items:
- main-comment steering and approval (`d1:206-256`), which Q4 removes;
- a conversation readable by every ticket reader (`d1:277`, `d1:389-390`, `d1:441-442`), which Q3 forbids;
- Q1: `require_kind = ""` turns the Human check off (`d1:233`, doctor `d1:555`), but agent comments must
  never steer;
- Q1: Relay is expected to give the product agent an owner (G-R3, `d1:131-133`, `d1:371`, `d1:444-445`, `d1:666`),
  but product agents have no owner by design, and personal agents already report one.

d1's owner-list rule itself (`d1:343-347`, `d1:358-365`) already matches the user's Q1 clarification.

**Fix:** revise into d2. The ATOMICS security model in §4 shrinks: checks 2-6 of `d1:356-367` stay as defence
in depth on `conversation_read` items, and the comment-mode rules go. State the user's Q1 rule in its own words in
§4.1: ATOMICS's connection identity (any agent) is separate from the steering humans (the owner list).

**H5. ATOMICS branches don't match Relay's development links, so ticket acceptance step 5 fails.** Relay links a
PR to a ticket by its head branch, `prefix/number-slug`; PR titles are not read, and the `#RELAY-<number> - `
commit-message form counts only on delivery-status events such as a squash-merge commit. ATOMICS's integration
branch is `atomics/plan-<id>` (`crates/atomics-core/src/plan.rs:513-521`), which never matches, and the repository
must also be connected on Relay's side.
- **Fix, ATOMICS side (generic):**
  - the Relay profile sets `[tickets] branch = "relay/{key_number}-{slug}"`, using the existing template
    support (`crates/atomics-core/src/tickets.rs:597-622`);
  - a ticket-mode plan's integration branch follows that template, so `relay/<number>-slug` and task branches
    `relay/<number>-slug-d1` match.
- Connecting the repositories is an operator step on Relay's side.
- Then `record_stage` (X5) is needed only for repositories outside the app; keep it optional.

### Medium

**M2. Author-kind values differ in case and vocabulary.** Comments report `Human`/`Agent`/`System` (live
`get_ticket`); `whoami` reports `human`/`personalAgent`/`agent`. d1 configures `require_kind = "human"`
(`d1:233`) but expects `"Human"` (`d1:363`). An exact compare fails closed: nothing ever steers. A quick fix to that
tends to disable the check, and d1 even offers `""` to disable it, which the user's rule ("comments from any agent
identity never steer") forbids. **Fix:**
- compare case-insensitively, or map values like `[tickets.states]`;
- an unmapped kind path disables remote steering rather than the check, and the doctor says why;
- test both casings in X2's table, and test that a personal agent's comment (kind `Agent`) never steers,
  even when its owner is on the list.

**M3. The plan digest doesn't cover what local approval covers.**
- `Plan::replace` keeps approval only when the goal and each task's id, title, brief, acceptance, deps, agent,
  args, isolation and repo are unchanged (`crates/atomics-core/src/plan.rs:425-445`).
- d1 hashes only ids, titles, deps and agents (`d1:203-204`, X3 step 1). So a rewritten brief, repo or args
  asks for approval again under the **same** digest.
- An `/approve` sent from a stale screen after the new request then approves work the owner never saw.
- **Fix:**
  - hash the goal plus exactly the `specification` fields of `Plan::replace`;
  - use 12 hex characters;
  - show each task's brief opening in the confirmation sheet;
  - X3 tests that changing a brief changes the digest.

**M4. Steering is more powerful than approval, and Q7's reasoning covers approval only.**
- A delivered steering message is the user's words. The chief then acts with its full local permissions,
  which in the user's setup include pushing branches, updating PRs, creating tickets and dispatching releases.
  Its Relay connection is the product agent, whose permissions are broad and sysadmin-like. So one phone message
  reaches more than a personal agent could.
- `Human` kind proves a human **credential**, not a keystroke. Signing in to MCP as a person (off on the user's
  Relay) or any human-owned API credential would write as the owner. That matters less once steering comes only
  from the owner's session (H1), but it belongs in §4.4.
- **Fix:**
  - in §4.3, list the actions that stay desktop-only: merges, releases, deploys, cluster mutations, secrets,
    deletions;
  - add the chief rule "a message *via relay* may steer and ask, but those actions need a local
    confirmation";
  - keep person sign-in off for remote use;
  - X2 tests that the label is present on every delivered message.

**M5. X1 overlaps the ticket-writes plan's running a1.**
- a1 (uncommitted, worktree `…<ticket>-a1-…`) already does all of this:
  - adds `{nonce}` to `PLACEHOLDERS`;
  - rejects `{nonce}` in read operations;
  - keeps a state push's nonce across heartbeat and sync retries;
  - retries a transport error once with the same nonce;
  - maps Relay `update_state` and `comment` with `idempotencyKey = "{nonce}"` (example profile in
    `config.rs`);
  - tests deduplication in both fake-server dialects.
- That is X1 steps 1 (nonce), 5 (`update_state`, `comment`) and 6 (`d1:512-522`).
- **Fix:** X1 depends on a1 merged. Keep only the `after`/`role`/`activity` placeholders, the `whoami`
  operation, `CommentPaths` identity fields, and the outbox. Drop the `create` mapping unless a task needs it.

**M6. Restart replay and outbox placement.**
- d1 saves the cursor in the plan file but doesn't say whether it is saved before or after the paste
  (`d1:247`). A crash in between either repeats or loses a message.
- **Fix:** adopt the steering rule of Relay's personal sessions ("persist delivery intent before sending;
  ambiguous sends warn instead of executing twice"):
  - record the message id as *delivering* before `send_to_chief`;
  - on restart, show "may not have reached the chief" instead of pasting again;
  - acknowledge to the server only after `Sent`.
- Also, `plan_json` sends the whole plan to the chief (`crates/atomics/src/app/plans.rs:351-354`). Keep
  `outbox`, `own_comments` and cursors out of it, with `serde(skip)` or a sidecar file.

**M7. Agent-writer comments containing `@alias` are refused.** If any mirror or ATOMICS text still goes to
main comments, a chief turn that contains a Relay agent alias (`@alice-agent`, say) is rejected. The outbox then
retries it ten times and drops it. **Fix:** with H1 this goes away. If any comment target remains, escape `@` in
mirrored text.

**M8. A sequence cursor can skip rows.** A comment's sequence is assigned at insert and visible at commit, so a
lower one that commits late is skipped by `seq > after`. **Fix:** any server cursor Relay adds must be
commit-ordered, or ATOMICS re-reads a short overlap and deduplicates by id.

**M9. The polling cost is unbounded on the server.**
- A fallback poll is a full `get_ticket`: the description, up to 200 comments, and a child count. MCP calls had no
  rate limit.
- **Fix:**
  - only a cursor operation may poll at 5 s, and the newest-N fallback polls at 60 s;
  - "active" means a chief turn or a remote message within 10 minutes, or a plan waiting for approval;
  - the 5 s cadence stops when the plan's chief session ends.

### Low

- **L2. Approve button contract.** Relay's frontend would parse ATOMICS's text to find the digest
  (`d1:466-468`). Document the approval event's format as part of the generic ATOMICS contract, or carry the
  digest in a structured event field.
- **L3. Test runs.** Full suites go through `atomics queue --class test` (`d1:487-488`, X6 `d1:726`).

M1, L1 and L4 concerned Relay's side only (which of its repositories a work item belongs to, its citations, and
its CI budget) and are kept with Relay.

### Agnosticism

ATOMICS code has no Relay names today. The word "relay" in Rust is the sandbox network relay
(`crates/atomics-core/src/isolation.rs`); the rest is the example profile (`config.rs:120-165`) and test
fixtures (`selfconfig.rs:967,1004`). a1 keeps Relay tool names in tests and the example profile only.

The design holds to that as long as:
- H1 is done through generic MCP operations, never through Relay's runner protocol;
- `require_kind` is mapped, not hard-coded (M2);
- the approval text format is ATOMICS's own contract (L2).

---

## 3. What the d2 task list should look like

| Task | Change from d1 |
| --- | --- |
| X1 | After a1: placeholders, `whoami` (`id`, `kind`, `owner_id` mapped; the owner is present only for personal agents), `CommentPaths` identity fields, outbox outside `plan_json` |
| X2 | `[tickets.remote]` with conversation steering only. `owners` = human user ids; empty means whoami's owner only when the connection is a personal agent. Verdict function: author ∈ owners, kind Human (mapped, never optional), author ≠ `whoami.id`, own ids, cursor. Delivery intent before paste. Doctor: remote off with "set owners" when the list is empty and whoami has no owner (the product agent); refuses owners that include `whoami.id` or a human connection (`whoami.kind == human`) |
| X3 | Digest over the `Plan::replace` fields, 12 hex characters; approval request and `/approve` inside the Conversation; no state approval |
| X4 | `conversation_*` operations; turns and tool blocks ≤ 2000 characters per event; activity states (needs you, waiting for approval, blocked) |
| X5 | Relay profile `branch` template and ticket-mode integration branch (H5); optional `record_stage`; no attention comments |
| X6 | Device acceptance, with Relay |

Relay's side (the conversation and its MCP tools, notices, push and its frontend) is planned with Relay. Wave A can
still start with X1 once the user approves d2.

---

## Resolved in d2

d2 revised the design in place, with the chief's fixed decisions (identities, H1–H5, the mediums). `§N` below means
a section of d2.

### High

| Finding | Outcome | Change in d2 |
| --- | --- | --- |
| H1 conversation needs a ticket link and an MCP surface | **Resolved in d2** | Relay's side builds an item conversation that any agent connection opens, visible and steerable only by its named steering humans, with MCP tools to open, post, read steering after a revision, acknowledge, and list the steering humans (§5.1). ATOMICS maps them as generic `conversation_*` operations (§3.1). Comment threads and `list_ticket_comments` are gone. Events are ≤ 2000 characters throughout (§3.6). |
| H2 push to the phone | **Resolved in d2** | A build on Relay's side (§5.3); ATOMICS needs nothing for it. |
| H3 attention comments can't notify; mention pushes leak text | **Resolved in d2** | **Chose the dedicated notification path** (§5.1.5): mention pushes put comment text on the lock screen, mentions live in main comments that every reader sees, and explicit MCP mentions would let any agent push to any human. Attention comments and `notify_mention` are deleted. Conversation activity (waiting for approval, waiting for input, failed) and Completed push to the steering humans with a generic title and no body. CI failed and deployed arrive as item notices, also with no body. |
| H4 the doc kept what Q3 and Q4 rejected; two Q1 details wrong | **Resolved in d2** | Gone: main-comment steering, `steer = "comments"`, `steer_prefix`, state approval (Option B), `target = "comments"` and `require_kind = ""`. The approval request and `/approve` live in the Conversation (§3.5). Only steering humans see the conversation (§4.2, §5.1.4). §4.1 states the user's Q1 rule in their words. G-R3 is deleted (whoami already reports a personal agent's owner). The chip names both identities (§3.2). The ATOMICS security model now covers only conversation items (§3.4). |
| H5 ATOMICS branches miss Relay's development links | **Resolved in d2** | The ATOMICS side is config only: `[tickets] branch = "relay/{key_number}-{slug}"` in the user's profile, using the existing template support, which already covers the ticket-mode integration branch and the `-<task>` suffix (§3.8). No Relay names in code. The board-mode integration branch can be passed to `plan_set`. Connecting the repositories is an operator step on Relay's side (§6). `record_stage` stays with the ticket-writes plan's A5. |

### Medium

| Finding | Outcome | Change in d2 |
| --- | --- | --- |
| M1 a work item aimed at the wrong repository | **Resolved in d2** | Relay's side only; nothing changes in ATOMICS. |
| M2 author-kind casing and vocabulary | **Resolved in d2** | `human_kind` is compared ignoring ASCII case, and `personal_agent_kind` reads whoami's value (§3.2). An unmapped author kind turns remote off with a reason; no setting disables the check. X2 tests `Human` and `human`, and that a personal agent's message never steers, even with its owner on the list. The server also refuses steering written by any agent (§4.2). |
| M3 the digest missed approval fields | **Resolved in d2** | The digest covers the goal plus exactly `Plan::replace`'s fields (id, title, brief, acceptance, depends_on, agent, args, isolation, repo), as 12 hex characters (§3.5). Plan events give the phone's sheet each task's agent, repo and brief opening. X3 tests that changing a brief, repo, args or agent changes the digest. |
| M4 steering is broader than approval | **Resolved in d2** | §4.4 lists the actions that stay confirmed at the desk and adds the chief's prompt rule (§3.6). Person sign-in stays off (§4.1). The residual risks are in §4.5. X2 tests the source label. |
| M5 X1 overlapped a1 | **Resolved in d2** | X1 depends on a1 merged to main, and keeps only what a1 didn't cover: whoami, comment identity fields, typed placeholders, the sidecar outbox and mention escaping (§6 X1). `create` stays unmapped. |
| M6 restart replay and outbox placement | **Resolved in d2** | Delivery intent is saved before `deliver`, the acknowledgement follows the outcome, and a restart never pastes again (§3.4). Remote state lives in a sidecar file outside `plan_json` (§3.3). |
| M7 `@alias` in agent-written comments is refused | **Resolved in d2** | The mirror never writes main comments. a1's lifecycle comments still do, so a generic `[tickets] escape_mentions` inserts U+2060 after `@`; the Relay profile sets it (§3.9). |
| M8 a sequence cursor can skip rows | **Resolved in d2** | The cursor is the steering revision, commit-ordered on Relay's side (§5.1.2). ATOMICS deduplicates by id as well (§3.4). |
| M9 unbounded polling cost | **Resolved in d2** | Only the cursor read polls, every 5 s while the plan is "active" (defined in §3.7) and every 60 s otherwise. Polling stops when the plan or the conversation ends, and backs off on 429. No `get_ticket` poll exists. Relay rate-limits MCP per identity (§5.1.7). |

### Low

| Finding | Outcome | Change in d2 |
| --- | --- | --- |
| L1 citations | **Resolved in d2** | Relay's side only. |
| L2 the Approve button's contract | **Resolved in d2** | A documented ATOMICS text contract: the Approval event's last line is `/approve <12 hex>`, and the Plan events since the latest Approval event list the tasks (§3.5). X3 documents and tests it; Relay's frontend reads it. |
| L3 test runs | **Resolved in d2** | Every task runs its full suite once through `atomics queue --class test` (§6). |
| L4 CI budget | **Resolved in d2** | Relay's side only. |

### The chief's answers (§1 table)

| Q | Outcome | Change in d2 |
| --- | --- | --- |
| Q1 owner identity | **Resolved in d2** | §4.1 states the user's rule. `owners` holds human user ids, and the empty default applies only to personal agents (§3.2). `require_kind` is gone and G-R3 is deleted. The chip is fixed, and the doctor says "set `[tickets.remote] owners`". The question is closed. |
| Q2 thread storage | **Resolved in d2** | See H1. Closed. |
| Q3 visibility | **Resolved in d2** | Steering humans only; `target = "comments"` is removed. Closed. |
| Q4 steering word | **Resolved in d2** | The Conversation only. Closed. |
| Q5 stages | **Resolved in d2** | See H5. Closed. |
| Q6 phase 2 | **Resolved in d2** | Approving permissions and merges from the phone stays phase 2 (§4.4). Closed. |
| Q7 session strength | **Resolved in d2** | No extra MFA in v1, plus the list of actions confirmed at the desk (§4.4). Closed. |
| Q8 frontend | **Resolved in d2** | Relay's frontend, built as a PWA (H2). Closed. |
| Q9 polling | **Resolved in d2** | See M9. Closed. |

### Agnosticism and the task list

- **Agnosticism: resolved in d2.** ATOMICS uses only generic MCP operations. Relay's vocabulary (tool names,
  event kinds, activity states, `itemType = "ticket"`) appears only in the example profile (§3.1, §3.2). The
  approval text is ATOMICS's own contract (§3.5).
- **Task list (§3 above): resolved in d2.** d1's X5 (attention comments and `record_stage`) is removed: branch
  linking is config (§3.8, O1), `record_stage` stays with the ticket-writes plan's A5, and attention became
  conversation activity (X4). d2's X5 is d1's X6. The work runs in five waves, to keep at most two build-heavy
  tasks at once.

Still open after d2: only the offline shell (d2 §7).
