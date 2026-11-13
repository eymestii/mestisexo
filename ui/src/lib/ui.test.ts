import { describe, expect, it } from "vitest";
import { webLink } from "./link";
import { levelFor } from "./quality.svelte";
import { colorFor, viewersLabel } from "./chat.svelte";

describe("formatos", () => {
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
