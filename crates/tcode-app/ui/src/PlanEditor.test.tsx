import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./Chips", () => ({
  ModelPicker: () => <button type="button">selected model</button>,
}));

import { PlanEditor } from "./PlanEditor";
import { draftOf, type Plan } from "./plan";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let container: HTMLDivElement;

const plan: Plan = {
  path: "plans/example.md",
  file: "example.md",
  title: "Example plan",
  description: "Keep this precise.",
  background: "## Context\n\n**Preserve** the existing workflow.",
  state: "draft",
  done: 0,
  total: 1,
  phases: [{ phase: "Carry it out", status: "pending", detail: "Do the work.", phases: [] }],
};

beforeEach(() => {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("PlanEditor", () => {
  it("renders read-only background as Markdown and makes the shared execution model explicit", () => {
    act(() => {
      root.render(
        <PlanEditor
          plan={plan}
          draft={draftOf(plan)}
          mode="review"
          onDraft={() => {}}
          onDecide={() => {}}
        />,
      );
    });

    const background = container.querySelector(".plan-background")!;
    expect(background.querySelector(".prose-h2")?.textContent).toBe("Context");
    expect(background.querySelector("strong")?.textContent).toBe("Preserve");
    expect(background.textContent).not.toContain("**");
    expect(container.textContent).toContain("Execution model");
    expect(container.textContent).toContain("Applies to every session in this app.");
    expect(container.textContent).toContain("selected model");
  });
});
