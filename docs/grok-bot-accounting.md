# Grok Bot token accounting

Grok Bot is a separate client (`grok-bot`). The existing `grok` client remains Grok Build. Tokens are read from the same API-derived Cursor JSON cache that Tokscale already synchronizes; this is not a parser for Cursor's code-tracking database.

## Classification and local agents

The parser recognizes UUID conversations containing a `grok-bot-*` router event, and `sand-subagent-<UUID>` conversation identifiers. All models in an anchored conversation retain their original model/token/cost data and are assigned to Grok Bot exactly once. Ordinary Grok-model activity in Cursor stays Cursor. Classification uses the complete cached event set before report date filtering. Legacy CSV lacks sufficient conversation evidence and remains Cursor.

Use `--client grok-bot` for Grok Bot reports, `--client cursor` for Cursor-only reports, or both for their combined total. They share one physical cache ingestion path. The local Agents view uses an account-qualified full conversation identifier as its fallback label. This is a conversation identity, **not a verified bot name or parent-bot relationship**. Missing relationships are not inferred from timestamps, model names, or display names.

Agent labels are local; ordinary `submit` still sends aggregate client/model/token/cost data, not bot names or conversation IDs. TUI exports may include the displayed agent labels and should be reviewed before sharing.

## Historical submissions

Reclassifying tokens already credited to Cursor must replace the Cursor/Grok Bot family atomically. A receiver supporting this feature requires Cursor submission generation 4 and Grok Bot generation 1, a full-history scan of both clients, and coverage of the device's credited day/model/token/message buckets. Partial or insufficient history keeps the previous family totals and returns a warning instead of adding the new Bot totals on top of old Cursor usage. Other clients and devices are unaffected.

Before uploading a payload that scans or contains either family member, the CLI checks public `GET /api/submit` capabilities for the exact atomic pair (Cursor 4, Grok Bot 1). A compatible receiver gets the original payload. A definitive older receiver (404/405 or a valid response without this capability) gets only unrelated clients: both Cursor and Grok Bot rows and generation keys are omitted, totals are rebuilt, and the CLI warns. If no unrelated rows remain, no POST occurs and the command returns an error, so autosubmit cannot record a successful upload. Previously credited family history stays unchanged, and local reports/exports are unaffected. Network, authentication, invalid-response, and server errors stop the upload rather than silently selecting legacy behavior.

Bot usage is never folded back into Cursor for compatibility: that could double-count a device that already migrated. Upgrade the receiver before uploading this family. This change does not deploy the official website or submit personal history automatically. Aggregate historical totals alone cannot recover Bot attribution when detailed events are gone.

## Evidence boundaries

The cache contains usage events, not a guaranteed one-to-one record of user requests or tool calls. Missing token fields in the existing normalized format become zero; absence is not evidence of zero consumption. Provider metered `totalCents` and wallet `chargedCents` have different meanings. Weekly allowance percentages from `GetSandUsageStatus` are separate from per-event token accounting and from the Grok/SuperGrok billing adapter.

The existing `usage --bots` command is a separate quota-oriented report and still contacts the quota API. Its pooled subtask/schedule breakdown is not a verified parent-child graph. This change does not claim to resolve Bot names, account entitlement linkage, cache completeness, or quota burn-rate accuracy.
