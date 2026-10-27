import { describe, expect, it } from "vitest";
import type { ClientBreakdownData, ModelBreakdownData } from "../../src/lib/db/helpers";
import {
  cursorLedgerPersistedVersion,
  foldGrokBotContributions,
  foldStoredGrokBot,
  grokBotPairDeclared,
  splitCursorUnion,
} from "../../src/lib/db/cursorGrokBot";

// Costs are in the ledger's quantized unit (1e-4), as parserHighWater stores them.
function model(input: number, cacheRead = 0, messages = 1, cost = input / 10_000): ModelBreakdownData {
  return { tokens: input + cacheRead, cost, input, output: 0, cacheRead, cacheWrite: 0, reasoning: 0, messages };
}
function cell(models: Record<string, ModelBreakdownData>): ClientBreakdownData {
  const total = Object.values(models).reduce((sum, m) => ({
    tokens: sum.tokens + m.tokens, cost: sum.cost + m.cost, input: sum.input + m.input, output: 0,
    cacheRead: sum.cacheRead + m.cacheRead, cacheWrite: 0, reasoning: 0, messages: sum.messages + m.messages,
  }), { tokens: 0, cost: 0, input: 0, output: 0, cacheRead: 0, cacheWrite: 0, reasoning: 0, messages: 0 });
  return { ...total, models };
}
function sum(...cells: Array<ClientBreakdownData | undefined>) {
  return {
    tokens: cells.reduce((s, c) => s + (c?.tokens ?? 0), 0),
    messages: cells.reduce((s, c) => s + (c?.messages ?? 0), 0),
    cost: cells.reduce((s, c) => s + (c?.cost ?? 0), 0),
  };
}

describe("Grok Bot wire contract", () => {
  it.each([
    [{ cursor: 2, "grok-bot": 1 }, true],
    [{ cursor: 4, "grok-bot": 1 }, false],
    [{ cursor: 3, "grok-bot": 1 }, false],
    [{ cursor: 2 }, false],
    [{ "grok-bot": 1 }, false],
    [{ cursor: 2, "grok-bot": 2 }, false],
    [undefined, false],
  ] as const)("accepts only the exact pair: %j -> %s", (versions, expected) => {
    expect(grokBotPairDeclared(versions as Record<string, number> | undefined)).toBe(expected);
  });

  it.each([
    [4, false, undefined],
    [4, true, 4],
    [2, false, 2],
    [3, false, 3],
    [undefined, false, undefined],
  ] as const)("maps only a ledgerless fork marker away: persisted %s, ledger %s", (persisted, hasLedger, expected) => {
    expect(cursorLedgerPersistedVersion(persisted, hasLedger)).toBe(expected);
  });
});

describe("folding Grok Bot into the Cursor union", () => {
  it("relabels only Grok Bot contributions", () => {
    const folded = foldGrokBotContributions([
      { date: "d", clients: [{ client: "grok-bot", modelId: "m" }, { client: "grok", modelId: "m" }] },
    ]);
    expect(folded[0].clients.map((c) => c.client)).toEqual(["cursor", "grok"]);
  });

  it("sums stored cells per model and keeps the originals", () => {
    const cursor = cell({ m: model(70) });
    const bot = cell({ m: model(30), b: model(5) });
    const { days, stored } = foldStoredGrokBot([{ date: "d", sourceBreakdown: { cursor, "grok-bot": bot, claude: cell({ c: model(1) }) } }]);
    const breakdown = days[0].sourceBreakdown as Record<string, ClientBreakdownData>;
    expect(Object.keys(breakdown).sort()).toEqual(["claude", "cursor"]);
    expect(breakdown.cursor.models.m.input).toBe(100);
    expect(breakdown.cursor.models.b.input).toBe(5);
    expect(stored.get("d")).toMatchObject({ cursor, bot });
  });

  it("leaves days without Grok Bot untouched", () => {
    const day = { date: "d", sourceBreakdown: { cursor: cell({ m: model(1) }) } };
    expect(foldStoredGrokBot([day]).days[0]).toBe(day);
  });
});

describe("splitting the Cursor union", () => {
  const union = cell({ m: model(100, 40, 4, 0.01), other: model(10) });

  it.each([
    ["evidence below the union", cell({ m: model(30, 10, 1, 0.002) })],
    ["evidence above the union in one field", cell({ m: model(30, 400, 9, 1) })],
    ["evidence for a model the union lacks", cell({ ghost: model(50) })],
  ])("always sums to the union exactly (%s)", (_label, evidence) => {
    const split = splitCursorUnion({ union, stored: undefined, snapshotBot: evidence, snapshotDatesFamily: true, partitionAccepted: true });
    const total = sum(split.cursor, split["grok-bot"]);
    expect(total.tokens).toBe(union.tokens);
    expect(total.messages).toBe(union.messages);
    expect(total.cost).toBeCloseTo(union.cost, 12);
    for (const part of [split.cursor, split["grok-bot"]]) {
      for (const m of Object.values(part?.models ?? {})) {
        expect(Math.min(m.input, m.cacheRead, m.messages, m.cost)).toBeGreaterThanOrEqual(0);
      }
    }
    expect(split["grok-bot"]?.models.ghost).toBeUndefined();
  });

  it("gives Grok Bot exactly its evidence when the union covers it", () => {
    const split = splitCursorUnion({ union, stored: undefined, snapshotBot: cell({ m: model(30, 10, 1, 0.002) }), snapshotDatesFamily: true, partitionAccepted: true });
    expect(split["grok-bot"]!.models.m).toMatchObject({ input: 30, cacheRead: 10, messages: 1 });
    expect(split.cursor!.models.m).toMatchObject({ input: 70, cacheRead: 30, messages: 3 });
    expect(split.cursor!.models.other.input).toBe(10);
  });

  it("keeps everything Cursor without any evidence", () => {
    expect(splitCursorUnion({ union, stored: undefined, snapshotBot: undefined, snapshotDatesFamily: true, partitionAccepted: true }))
      .toEqual({ cursor: union });
  });

  const storedCursor = cell({ m: model(70) });
  const storedBot = cell({ m: model(30) });
  const stored = { cursor: storedCursor, bot: storedBot, folded: cell({ m: model(100, 0, 2) }) };

  it("writes back an unchanged union verbatim unless an accepted snapshot re-attributes it", () => {
    const unchanged = cell({ m: model(100, 0, 2) });
    const frozen = splitCursorUnion({ union: unchanged, stored, snapshotBot: cell({ m: model(90) }), snapshotDatesFamily: true, partitionAccepted: false });
    expect(frozen.cursor).toBe(storedCursor);
    expect(frozen["grok-bot"]).toBe(storedBot);
    const accepted = splitCursorUnion({ union: unchanged, stored, snapshotBot: cell({ m: model(90) }), snapshotDatesFamily: true, partitionAccepted: true });
    expect(accepted["grok-bot"]!.models.m.input).toBe(90);
    expect(accepted.cursor!.models.m.input).toBe(10);
  });

  it("uses stored attribution when the snapshot does not date the family", () => {
    const grown = cell({ m: model(150, 0, 2) });
    const split = splitCursorUnion({ union: grown, stored, snapshotBot: undefined, snapshotDatesFamily: false, partitionAccepted: true });
    expect(split["grok-bot"]!.models.m.input).toBe(30);
    expect(split.cursor!.models.m.input).toBe(120);
  });

  it("negative control: an unattributed legacy remainder is never guessed as Grok Bot", () => {
    const legacy: ClientBreakdownData = { ...cell({}), tokens: 50, input: 50, messages: 1, cost: 0.1, modelId: undefined };
    const split = splitCursorUnion({ union: legacy, stored: undefined, snapshotBot: cell({ m: model(50) }), snapshotDatesFamily: true, partitionAccepted: true });
    expect(split["grok-bot"]).toBeUndefined();
    expect(split.cursor!.models.unknown.input).toBe(50);
  });
});
