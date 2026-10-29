# Grok Bot token accounting

Grok Bot is a separate client (`grok-bot`). The existing `grok` client remains Grok Build. Tokens are read from the same API-derived Cursor JSON cache that Tokscale already synchronizes; this is not a parser for Cursor's code-tracking database.

## Classification and local agents

The parser recognizes UUID conversations containing a `grok-bot-*` router event, and `sand-subagent-<UUID>` conversation identifiers. All models in an anchored conversation retain their original model/token/cost data and are assigned to Grok Bot exactly once. Ordinary Grok-model activity in Cursor stays Cursor. Classification uses the complete cached event set before report date filtering. Legacy CSV lacks sufficient conversation evidence and remains Cursor.

Use `--client grok-bot` for Grok Bot reports, `--client cursor` for Cursor-only reports, or both for their combined total. They share one physical cache ingestion path. The local Agents view uses an account-qualified full conversation identifier as its fallback label. This is a conversation identity, **not a verified bot name or parent-bot relationship**. Missing relationships are not inferred from timestamps, model names, or display names.

Agent labels are local; ordinary `submit` still sends aggregate client/model/token/cost data, not bot names or conversation IDs. TUI exports may include the displayed agent labels and should be reviewed before sharing.

## Historical submissions

Grok Bot is a partition of the Cursor ledger, not a ledger of its own (see `grok-bot-upstream-alignment.md`). Receivers credit Cursor + Grok Bot as upstream's Cursor generation 2 (high-water, per-day growth, recount proof over every Cursor-cache row) and only then split each written Cursor cell into Cursor and Grok Bot. Totals therefore always equal what an un-split Cursor submission credits; a reclassified row is never counted twice, and history the snapshot no longer contains keeps the attribution it was stored with.

Before uploading a payload that scans or contains either family member, the CLI checks public `GET /api/submit` capabilities. A receiver advertising exactly `cursorGrokBot.parserVersions = {cursor: 2, grok-bot: 1}` gets the split payload. A receiver with no Grok Bot contract (404/405, or a valid response without `cursorGrokBot`, such as tokscale.ai) gets Grok Bot folded into Cursor, exactly what an un-split CLI sends. A receiver advertising any other Grok Bot contract (for example the earlier fork `cursor: 4`) gets neither: both clients are omitted, totals are rebuilt, and the CLI warns; if nothing else remains the command fails so autosubmit cannot record a successful upload. Network, authentication, invalid-response, and server errors stop the upload rather than silently selecting a fallback.

A receiver freezes the whole family for any submit that names Grok Bot without declaring that exact pair (older fork CLIs, bot-only scans, imports). Upgrade the receiver before the CLI. Aggregate historical totals alone cannot recover Bot attribution when detailed events are gone.

## Evidence boundaries

The cache contains usage events, not a guaranteed one-to-one record of user requests or tool calls. Missing token fields in the existing normalized format become zero; absence is not evidence of zero consumption. Provider metered `totalCents` and wallet `chargedCents` have different meanings. Weekly allowance percentages from `GetSandUsageStatus` are separate from per-event token accounting and from the Grok/SuperGrok billing adapter.

This change does not claim to resolve Bot names, account entitlement linkage, or cache completeness.

