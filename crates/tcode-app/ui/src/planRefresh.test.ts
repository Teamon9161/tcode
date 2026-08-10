import { describe, expect, it } from "vitest";

import { PlanRefreshes } from "./planRefresh";

describe("PlanRefreshes", () => {
  it("keeps a late older plan response from replacing the newest state", () => {
    const refreshes = new PlanRefreshes();

    const first = refreshes.begin("session-a");
    const second = refreshes.begin("session-a");

    expect(refreshes.isCurrent("session-a", second)).toBe(true);
    expect(refreshes.isCurrent("session-a", first)).toBe(false);
  });

  it("orders refreshes independently for each session", () => {
    const refreshes = new PlanRefreshes();
    const firstA = refreshes.begin("session-a");
    const firstB = refreshes.begin("session-b");

    refreshes.begin("session-a");

    expect(refreshes.isCurrent("session-a", firstA)).toBe(false);
    expect(refreshes.isCurrent("session-b", firstB)).toBe(true);
  });
});
