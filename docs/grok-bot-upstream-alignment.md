# Grok Bot accounting on upstream's Cursor ledger

Status: implemented on `feat/grok-bot-upstream-aligned`. Replaces the fork-only
`cursor: 4` / `grok-bot: 1` atomic family transition (`grokBotTransition.ts`).

## Why change it

Upstream now heals inflated Cursor history with a recount proof at Cursor
generation 2 (#1552) and grows Cursor per day (#1553). The fork kept a
parallel scheme for the same rows: generation 4, a coverage-gated replace of
the Cursor/Grok Bot family, and no recount proof. After merging upstream the
receiver held both, and they contradicted each other:

- `cursor: 4` meant two different counts. Fork CLIs up to `v4.19.1-dnplus.3`
  counted every running snapshot of a Cursor event (upstream generation-1
  units); `v4.20.1-dnplus.1` collapses snapshots (#1544, generation-2 units).
  Neither sends a recount proof, so the receiver cannot tell which units it
  stored.
- The family replace requires the snapshot to cover every credited
  (day, model) bucket. Collapsed snapshots count lower than the inflated
  buckets they must cover, so an affected device froze forever, and archived
  Cursor caches (`tokscale cursor logout`) froze it the same way. #1553 exists
  to stop exactly that for Cursor.
- A device the fork transitioned carries `parserVersions.cursor = 4` without a
  Cursor high-water state. Upstream's planner treats "marker without state" as
  corruption and freezes, so those devices could never reach generation 2.

## Design

Grok Bot is a **presentation partition of the Cursor ledger**, not a ledger.

1. The receiver folds `grok-bot` into `cursor` (stored cells and the incoming
   snapshot), runs upstream's Cursor generation-2 planner unchanged on the
   union (high-water state, per-day growth, recount proof), and merges.
2. Each written Cursor union cell is split back into `cursor` and `grok-bot`.
   For every (model, token field / messages / cost) Grok Bot gets
   `min(union, evidence)` and Cursor the remainder, so the two always sum to
   the union. Evidence is this snapshot's Grok Bot cell when the submit
   declared the partition, the Cursor ledger accepted it (not frozen) and the
   snapshot dates Cursor usage that day; otherwise it is the stored Grok Bot
   cell. A union a frozen or un-split submit leaves unchanged is written back
   verbatim.

Consequences:

- Totals on every receiver equal what upstream credits for un-split Cursor.
  The split can only ever be wrong in attribution, never in totals, and a
  reclassified row (cursor -> bot) is never credited twice because there is
  only one ledger.
- No new Cursor generation is needed: the union is counted exactly like
  upstream generation 2, and the `parserStates.cursor` ledger stays in
  upstream units. A device can switch between this CLI and an upstream CLI.
- History that is not in the snapshot keeps the attribution it was stored
  with; un-split legacy usage stays Cursor (an unattributed `unknown`
  remainder is never guessed as Grok Bot).

### Wire contract

| Field | Value |
|---|---|
| `scanScope.parserVersions.cursor` | `2` (upstream's Cursor ledger generation) |
| `scanScope.parserVersions["grok-bot"]` | `1`, only when the payload carries the partition |
| `scanScope.recountProofs.cursor` | one proof over **all** Cursor-cache rows, Grok Bot included, keyed `date -> model`. Its cells are filtered to the submitted Cursor + Grok Bot cells, so it matches the union snapshot. There is no `grok-bot` proof. |
| `GET /api/submit` | `{"capabilities":{"cursorGrokBot":{"parserVersions":{"cursor":2,"grok-bot":1}}}}` |

Receiver rule: a submit that names `grok-bot` anywhere (rows or declaration)
must declare exactly `{cursor: 2, "grok-bot": 1}` and not be a backfill; any
other shape freezes the whole family (stored Cursor and Grok Bot unchanged,
warning returned). `cursor: 2` alone is an ordinary un-split upstream submit.

CLI rule (`submit_compat.rs`), from the capability:

| Receiver advertises | CLI sends |
|---|---|
| `cursorGrokBot` = exactly `{cursor:2, grok-bot:1}` | **Split**: payload as built |
| no `cursorGrokBot` (or 404/405) | **Fold**: Grok Bot rows, declarations, retention floor and sessions relabelled Cursor; same-model cells summed. This is byte-for-byte the shape an un-split CLI sends. |
| `cursorGrokBot` with any other value | **Strip**: both clients withheld (fail closed); error if nothing else remains |
| network/auth/5xx/invalid body | abort upload (unchanged) |

### Moving a device from un-split Cursor to the split

The first full-history pair submit is the Cursor generation-2 transition of
upstream, on the union:

- Un-split rows (stored as `cursor`) and any earlier split (stored as `cursor`
  + `grok-bot`) are folded into one union cell per day.
- The union recount proof lowers a stored (day, model) cell only when its
  counts equal exactly what generation 1 counted from the same rows
  (upstream's rule); otherwise the cell is defended.
- `baseline-legacy` credits only per-day growth beyond the corrected cells.
- The split then attributes the result. Nothing that was credited is added
  again, and nothing that cannot be proven a duplicate is removed.

A device with only the fork marker (`parserVersions.cursor = 4`, no
`parserStates.cursor`) is treated as "before the Cursor ledger transition":
that marker was only written by the fork's family replace, whose stored rows
are the credited ledger. Any other marker without a state still freezes, as
upstream requires. After the transition `parserVersions.cursor` is `2`; the
leftover `grok-bot: 1` key is inert.

## Compatibility matrix

CLIs:

- **F1**: fork CLI `v4.18.0-dnplus.1` ... `v4.19.1-dnplus.3` — `cursor:4 +
  grok-bot:1`, generation-1 counting, no proof, requires capability `{4,1}`.
- **F2**: fork CLI `v4.20.1-dnplus.1` — same declaration, generation-2
  counting, no proof, requires `{4,1}`.
- **N**: this CLI — `cursor:2 (+ grok-bot:1)`, union proof, Split/Fold/Strip.
- **U**: upstream CLI — `cursor:2`, un-split, proof (shown for reference).

Receivers:

- **P0**: currently deployed PA Cafe (fork `main`): advertises `{4,1}`,
  `grokBotTransition` family replace, no Cursor high-water.
- **P1**: PA Cafe from this branch.
- **T**: tokscale.ai (upstream): no `GET /api/submit` (405), no `grok-bot`
  client, Cursor generation 2 with recount proof.

| CLI \ receiver | P0 (deployed PA Cafe) | P1 (new PA Cafe) | T (tokscale.ai) |
|---|---|---|---|
| F1 | Sends split. Family replace when the snapshot covers every credited bucket, else freeze. Totals never decrease; inflated (gen-1) units may be stored. Unchanged by this PR. | Capability `{2,1}` != `{4,1}` -> CLI strips Cursor + Grok Bot; other clients upload. Stored family unchanged (frozen until the CLI is upgraded). Correct, nothing re-credited. If a `cursor:4` payload still arrived, the receiver freezes the family. | 405 -> CLI strips family (existing behaviour). Unchanged. |
| F2 | As F1, but collapsed counts often fail coverage of inflated stored buckets -> frozen. Unchanged by this PR. | Same as F1: family withheld by the CLI, receiver would freeze it anyway. | Same as F1. |
| N | Capability `{4,1}` -> **Strip**: family withheld, other clients upload, warning "different parser generation". Stored totals unchanged. | **Split**. Union ledger at generation 2; first full submit is the recount-proof transition (fork marker devices included); then per-day growth; split per day. Totals = upstream's; attribution from the snapshot. | 405 -> **Fold**: sends exactly what U sends. Upstream heals/grows Cursor as for any user. Totals correct; Grok Bot shows as Cursor there. Previously this CLI withheld Cursor entirely. |
| U | Un-split `cursor:2`. P0's family logic: before any split evidence status-quo merge; after a split -> freeze. Unchanged. | Ordinary upstream Cursor submit on the union. Growth goes to Cursor, stored Grok Bot attribution kept. Totals correct. | Native. |

Device states on P1 for an N submit:

| Stored state | Result |
|---|---|
| New device | `baseline-new` on the union, split from the snapshot. |
| Un-split `cursor` rows, no marker (or `cursor:1`) | Upstream transition on the union: proof heals exact gen-1 duplicates, bounded growth, then split. |
| Fork split (`cursor`+`grok-bot` cells, marker `cursor:4`, no state) | Same transition; marker ignored only in this exact case. Not frozen, nothing re-credited (covered by test). |
| Generation-2 ledger already established | Incremental per-day growth on the union; split re-attributes accepted days. |
| Unknown marker without state (e.g. `cursor:3`) / invalid state | Freeze (upstream's fail-closed rule, kept; negative control test). |
| Partial scan, import/backfill, bot-only scan, `cursor:4`, undeclared bot rows | Freeze the family; stored cells and attribution unchanged; unrelated clients advance. |

### Can P1 accept F1/F2 safely?

Not in general, so it fails closed. `cursor:4` does not say which counting
generation produced it and carries no proof, so P1 cannot heal or bound it
against a generation-2 ledger; accepting it after the transition would mix
units in the ledger, and accepting it before would need P1 to keep advertising
`{4,1}`, which would also make every F1/F2 CLI keep sending split payloads P1
cannot verify. The cost: Cursor/Grok Bot usage from an un-upgraded fork CLI is
not uploaded to PA Cafe until that CLI is upgraded. No stored history is lost
or re-credited, and other clients keep uploading.

## PA Cafe changes and rollout order

Receiver (`packages/frontend`):

- `src/lib/db/cursorGrokBot.ts` (new): wire constants, fold/split, fork marker
  rule. Replaces `grokBotTransition.ts`.
- `src/app/api/submit/route.ts`: advertise `{cursor:2, grok-bot:1}`; fold
  stored and incoming Grok Bot into Cursor before planning; family-freeze
  guard; split each written Cursor cell; ignore the fork marker only when
  there is no Cursor state.
- No schema migration and no batch rewrite. Stored rows change only when a
  device submits, by the same per-submit rules as upstream.

Order (server before CLI, as upstream does):

1. Deploy P1 (PA Cafe). From this moment F1/F2 CLIs stop uploading the
   Cursor/Grok Bot family to PA Cafe (fail closed, warning) but keep uploading
   everything else; tokscale.ai is unaffected.
2. Release the N CLI and upgrade every machine. Each device's first
   full-history submit performs its generation-2 transition on PA Cafe.
   On tokscale.ai the same CLI now uploads Cursor (folded).
3. Releasing N before P1 is safe but useless for PA Cafe: N strips the family
   for P0 until P1 is deployed.

## What could go upstream

- `crates/tokscale-core` proof over all Cursor-cache rows is a no-op upstream
  (no split there) and can stay fork-only.
- The upstream-able unit is the whole partition: the `grok-bot` client
  (classification in `sessions/cursor.rs`), the receiver fold/split module
  and route hooks, the CLI Split/Fold/Strip negotiation, and the tests below.
  It touches no upstream ledger code (`parserHighWater.ts` is unchanged), so
  it can be proposed as one single-topic PR after the client itself.

## Tests

- `packages/frontend/__tests__/lib/cursorGrokBot.test.ts`: pair matrix, marker
  matrix, fold, split sum invariant over a selection of evidence shapes,
  verbatim write-back vs accepted re-attribution, negative control for the
  unattributed remainder.
- `packages/frontend/__tests__/api/submitMiCodeTransition.test.ts` (route,
  real merge/SQL transport): new device, un-split -> split without re-adding
  the bot share, union recount heal and its no-proof negative control, fork
  marker transition and its unknown-marker negative control, a six-way freeze
  matrix with unrelated growth, partial pair, upstream un-split submit after a
  split, reclassification with and without growth.
- `crates/tokscale-cli/src/submit_compat.rs`: Split/Fold/Strip capability
  matrix, fold payload shape and totals, bot-only fold, strip-only error.
- `crates/tokscale-core/src/lib.rs`: Cursor proof covers rows classified as
  Grok Bot.
