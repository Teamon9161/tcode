import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ProgressStrip } from "./ProgressStrip";
import type { Plan } from "./plan";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let container: HTMLDivElement;

const plan: Plan = {
  path: "plans/example.md",
  file: "example.md",
  title: "Example plan",
  description: "Keep this precise.",
  background: "## Context\n\n**Preserve** the existing workflow.",
  state: "active",
  done: 0,
  total: 1,
  phases: [{ phase: "Carry it out", status: "in_progress", detail: "Do the work.", phases: [] }],
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

describe("ProgressStrip", () => {
  it("keeps plan background behind its own disclosure in the expanded strip", () => {
    act(() => {
      root.render(<ProgressStrip plan={plan} expanded onToggle={() => {}} onOpen={() => {}} />);
    });

    const disclosure = [...container.querySelectorAll("button")].find(
      (button) => button.textContent?.includes("Background"),
    )!;
    expect(disclosure.getAttribute("aria-expanded")).toBe("false");
    expect(container.querySelector(".strip-background .strip-detail")).toBeNull();

    act(() => disclosure.click());

    const detail = container.querySelector(".strip-background .strip-detail")!;
    expect(disclosure.getAttribute("aria-expanded")).toBe("true");
    expect(detail.querySelector(".prose-h2")?.textContent).toBe("Context");
    expect(detail.querySelector("strong")?.textContent).toBe("Preserve");
    const scroller = container.querySelector(".strip-expanded")!;
    expect(scroller.querySelector(".strip-background .strip-detail")).toBe(detail);
    expect(scroller.querySelector(".strip-phases")).not.toBeNull();
  });

  it("does not render the background disclosure while the strip is collapsed", () => {
    act(() => {
      root.render(
        <ProgressStrip plan={plan} expanded={false} onToggle={vi.fn()} onOpen={vi.fn()} />,
      );
    });

    expect(container.textContent).not.toContain("Background");
  });
});
