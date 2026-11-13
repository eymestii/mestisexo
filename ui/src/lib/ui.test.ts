import { describe, expect, it } from "vitest";
import { fmtTime, health, webLink } from "./health";
import { levelFor } from "./quality.svelte";
import { colorFor, viewersLabel } from "./chat.svelte";
import type { Status } from "./types";

const base: Status = {
  live: true, viewers: 10, chat_messages: 0, status: "", tunnel: null, tunnel_up: true,
  live_since: 1000, bytes_total: 1_000_000, frags_total: 0, last_data_ms: 10_000, now_ms: 10_000,
  quality_poor_now: 0, quality_avg_bandwidth: 0,
};

describe("health", () => {
  it("solo muestra la píldora de calidad si hay alguien con problemas", () => {
    expect(health(base).quality).toBeNull();
    const q = health({ ...base, quality_poor_now: 4 }).quality!;
    expect(q.text).toBe("40% con lag");
    expect(q.cls).toBe("bad");
  });
});

describe("formatos", () => {
  it("fmtTime", () => {
    expect(fmtTime(65_000)).toBe("1:05");
    expect(fmtTime(3_725_000)).toBe("1:02:05");
  });

  it("webLink convierte la IP en sslip.io", () => {
    expect(webLink("stream@109.123.252.126")).toBe("https://109-123-252-126.sslip.io/");
    expect(webLink("")).toBeNull();
  });

  it("viewersLabel", () => {
    expect(viewersLabel(1)).toBe("1 espectador");
    expect(viewersLabel(2)).toBe("2 espectadores");
  });

  it("colorFor es estable", () => {
    expect(colorFor("ana")).toBe(colorFor("ana"));
  });
});

describe("levelFor", () => {
  it("va mal sin búfer o con un corte reciente", () => {
    expect(levelFor(0.2, 0, 100_000)).toBe("poor");
    expect(levelFor(3, 90_000, 100_000)).toBe("poor");
    expect(levelFor(1, 0, 100_000)).toBe("fair");
    expect(levelFor(3, 50_000, 100_000)).toBe("good");
  });
});
