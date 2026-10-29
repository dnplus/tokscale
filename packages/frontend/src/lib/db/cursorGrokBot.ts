import { deriveClientBreakdownProvenance, type ClientBreakdownData, type ModelBreakdownData } from "./helpers";
import { modelsForHighWater, SUPPORTED_VERSIONED_PARSERS } from "./parserHighWater";
import { createSafeRecord, ownValue } from "../safeRecord";

/**
 * Grok Bot is a presentation partition of the Cursor ledger, not a ledger of
 * its own. Both are read from the same Cursor usage caches, and a row can be
 * reclassified between them, so independent per-client guards would credit a
 * reclassified row twice. The receiver therefore folds `grok-bot` into
 * `cursor`, runs the ordinary Cursor generation (high-water, per-day growth,
 * recount proof) on the union, and only then splits each written Cursor cell
 * back into Cursor and Grok Bot. Totals are exactly what an un-split Cursor
 * submission would have credited; only attribution depends on the split.
 *
 * `cursor` is the Cursor ledger generation, `grok-bot` declares that the
 * payload carries the partition. A submission touching `grok-bot` must declare
 * exactly this pair or the whole family freezes.
 */
export const GROK_BOT_PARSER_VERSION = 1;
export const CURSOR_GROK_BOT_PARSER_VERSIONS: Readonly<Record<"cursor" | "grok-bot", number>> = {
  cursor: SUPPORTED_VERSIONED_PARSERS.cursor,
  "grok-bot": GROK_BOT_PARSER_VERSION,
};

/**
 * Generation marker written by the earlier fork-only atomic family transition
 * (`cursor: 4`, `grok-bot: 1`). It never came with a Cursor high-water state:
 * the stored rows themselves were the credited ledger. A device carrying only
 * this marker is therefore still before the Cursor ledger transition.
 */
export const LEGACY_SPLIT_CURSOR_MARKER = 4;

type Contribution = { date: string; clients: Array<{ client: string; modelId: string }> };
type StoredDay = { date: string; sourceBreakdown: unknown };
type ParserVersions = Record<string, number> | null | undefined;

export function submissionTouchesGrokBot(
  contributions: readonly Contribution[],
  parserVersions: ParserVersions,
): boolean {
  return ownValue(parserVersions ?? {}, "grok-bot") !== undefined ||
    contributions.some((day) => day.clients.some((cell) => cell.client === "grok-bot"));
}

export function grokBotPairDeclared(parserVersions: ParserVersions): boolean {
  return ownValue(parserVersions ?? {}, "cursor") === CURSOR_GROK_BOT_PARSER_VERSIONS.cursor &&
    ownValue(parserVersions ?? {}, "grok-bot") === CURSOR_GROK_BOT_PARSER_VERSIONS["grok-bot"];
}

/** The persisted Cursor generation the Cursor ledger planner should see. */
export function cursorLedgerPersistedVersion(
  persisted: number | undefined,
  hasLedgerState: boolean,
): number | undefined {
  return persisted === LEGACY_SPLIT_CURSOR_MARKER && !hasLedgerState ? undefined : persisted;
}

/** Contributions with every Grok Bot cell relabelled as Cursor. */
export function foldGrokBotContributions<T extends Contribution>(contributions: readonly T[]): T[] {
  return contributions.map((day) =>
    day.clients.some((cell) => cell.client === "grok-bot")
      ? { ...day, clients: day.clients.map((cell) => cell.client === "grok-bot" ? { ...cell, client: "cursor" } : cell) }
      : day
  );
}

const TOKEN_FIELDS = ["input", "output", "cacheRead", "cacheWrite", "reasoning"] as const;

function cellFromModels(models: Record<string, ModelBreakdownData>, costIsComplete: boolean): ClientBreakdownData {
  const cell: ClientBreakdownData = {
    tokens: 0, cost: 0, input: 0, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0, messages: 0, models,
  };
  for (const model of Object.values(models)) {
    for (const field of TOKEN_FIELDS) cell[field] += model[field];
    cell.tokens += model.tokens;
    cell.cost += model.cost;
    cell.messages += model.messages;
  }
  const provenance = deriveClientBreakdownProvenance(cell);
  cell.provenance = costIsComplete ? provenance : { ...provenance, costIsComplete: false };
  return cell;
}

function emptyModel(): ModelBreakdownData {
  return { tokens: 0, cost: 0, input: 0, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0, messages: 0 };
}

function isEmpty(model: ModelBreakdownData): boolean {
  return model.tokens === 0 && model.messages === 0 && model.cost === 0;
}

function costComplete(...cells: Array<ClientBreakdownData | undefined>): boolean {
  return cells.every((cell) => cell?.provenance?.costIsComplete !== false);
}

function combine(a: ClientBreakdownData | undefined, b: ClientBreakdownData | undefined): ClientBreakdownData {
  const models = modelsForHighWater(a);
  for (const [id, model] of Object.entries(modelsForHighWater(b))) {
    const sum = { ...(ownValue(models, id) ?? emptyModel()) };
    for (const field of TOKEN_FIELDS) sum[field] += model[field];
    sum.tokens += model.tokens;
    sum.cost += model.cost;
    sum.messages += model.messages;
    if (sum.inputIncludingCacheRead != null || model.inputIncludingCacheRead != null) {
      sum.inputIncludingCacheRead = (sum.inputIncludingCacheRead ?? 0) + (model.inputIncludingCacheRead ?? 0);
    }
    models[id] = sum;
  }
  return cellFromModels(models, costComplete(a, b));
}

/** The stored Cursor/Grok Bot cells of one day, as written before this submit. */
export interface StoredFamilyCells {
  cursor?: ClientBreakdownData;
  bot: ClientBreakdownData;
  folded: ClientBreakdownData;
}

/**
 * Fold stored Grok Bot cells into Cursor so the Cursor ledger sees the union
 * it would have stored without the split. The originals are returned per date
 * so a day whose union the submit leaves unchanged is written back verbatim.
 */
export function foldStoredGrokBot<T extends StoredDay>(days: readonly T[]): {
  days: T[];
  stored: Map<string, StoredFamilyCells>;
} {
  const stored = new Map<string, StoredFamilyCells>();
  const folded = days.map((day) => {
    const breakdown = (day.sourceBreakdown ?? {}) as Record<string, ClientBreakdownData>;
    const bot = ownValue(breakdown, "grok-bot");
    if (!bot) return day;
    const cursor = ownValue(breakdown, "cursor");
    const union = combine(cursor, bot);
    stored.set(day.date, { cursor, bot, folded: union });
    const rest = createSafeRecord<ClientBreakdownData>();
    for (const [client, cell] of Object.entries(breakdown)) {
      if (client !== "grok-bot" && client !== "cursor") rest[client] = cell;
    }
    rest.cursor = union;
    return { ...day, sourceBreakdown: rest } as T;
  });
  return { days: folded, stored };
}

const COST_EPSILON = 1e-9;

function sameCounts(a: ClientBreakdownData, b: ClientBreakdownData): boolean {
  const left = modelsForHighWater(a);
  const right = modelsForHighWater(b);
  const ids = new Set([...Object.keys(left), ...Object.keys(right)]);
  for (const id of ids) {
    const x = ownValue(left, id) ?? emptyModel();
    const y = ownValue(right, id) ?? emptyModel();
    if (TOKEN_FIELDS.some((field) => x[field] !== y[field]) || x.messages !== y.messages ||
      Math.abs(x.cost - y.cost) > COST_EPSILON) return false;
  }
  return a.provenance?.costIsComplete === b.provenance?.costIsComplete;
}

/**
 * Split one written Cursor union cell into Cursor and Grok Bot.
 *
 * Each (model, field) of the union gives Grok Bot at most what the bot
 * evidence holds, and Cursor the remainder. The evidence is this snapshot's
 * Grok Bot cell only when the submit was accepted with the declared partition
 * and dates Cursor usage that day; otherwise it is the previously stored Grok
 * Bot cell, so a frozen, un-split (upstream or `--client cursor`) or partial
 * submit keeps earlier attribution. A union such a submit leaves unchanged is
 * written back verbatim.
 *
 * The two outputs always sum to the union exactly: attribution can be wrong
 * only where the evidence is, never the total.
 */
export function splitCursorUnion(args: {
  union: ClientBreakdownData | undefined;
  stored: StoredFamilyCells | undefined;
  snapshotBot: ClientBreakdownData | undefined;
  snapshotDatesFamily: boolean;
  /** The submit declared the partition and the Cursor ledger accepted it. */
  partitionAccepted: boolean;
}): { cursor?: ClientBreakdownData; "grok-bot"?: ClientBreakdownData } {
  const { union, stored } = args;
  if (!union) return {};
  const fromSnapshot = args.partitionAccepted && args.snapshotDatesFamily;
  if (!fromSnapshot && stored && sameCounts(union, stored.folded)) {
    return { ...(stored.cursor ? { cursor: stored.cursor } : {}), "grok-bot": stored.bot };
  }
  const evidence = fromSnapshot ? args.snapshotBot : stored?.bot;
  if (!evidence) return { cursor: union };
  const evidenceModels = modelsForHighWater(evidence);
  const botModels = createSafeRecord<ModelBreakdownData>();
  const cursorModels = createSafeRecord<ModelBreakdownData>();
  for (const [id, model] of Object.entries(modelsForHighWater(union))) {
    const limit = ownValue(evidenceModels, id) ?? emptyModel();
    const bot = emptyModel();
    const rest = emptyModel();
    for (const field of TOKEN_FIELDS) {
      bot[field] = Math.min(model[field], limit[field]);
      rest[field] = model[field] - bot[field];
    }
    bot.tokens = TOKEN_FIELDS.reduce((sum, field) => sum + bot[field], 0);
    rest.tokens = model.tokens - bot.tokens;
    bot.messages = Math.min(model.messages, limit.messages);
    rest.messages = model.messages - bot.messages;
    bot.cost = Math.min(model.cost, limit.cost);
    rest.cost = model.cost - bot.cost;
    if (!isEmpty(bot)) botModels[id] = bot;
    if (!isEmpty(rest)) cursorModels[id] = rest;
  }
  const complete = costComplete(union);
  return {
    ...(Object.keys(cursorModels).length > 0 ? { cursor: cellFromModels(cursorModels, complete) } : {}),
    ...(Object.keys(botModels).length > 0 ? { "grok-bot": cellFromModels(botModels, complete) } : {}),
  };
}

/** Replace a breakdown's Cursor union with its split, in place. */
export function applyCursorSplit(
  breakdown: Record<string, ClientBreakdownData>,
  split: ReturnType<typeof splitCursorUnion>,
): void {
  delete breakdown.cursor;
  delete breakdown["grok-bot"];
  if (split.cursor) breakdown.cursor = split.cursor;
  if (split["grok-bot"]) breakdown["grok-bot"] = split["grok-bot"];
}
