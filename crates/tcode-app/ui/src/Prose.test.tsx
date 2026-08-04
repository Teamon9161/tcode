import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { LinkContext, Prose } from "./Prose";

/**
 * Links in prose, connected to the window.
 *
 * The mechanism under test is delegation: `rich()` is cached on its source text
 * and cannot carry a callback (rule 21), so the handler lives on the container
 * and reads the anchor. What that buys — and what breaks quietly if anyone
 * "simplifies" it — is asserted here: the raw `href`, not the one the DOM
 * resolves against the app's own URL, and a click that never reaches the
 * document's default navigation.
 */

// React's `act` warns without this flag, and the warning is noise here.
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let container: HTMLDivElement;
const follow = vi.fn();

beforeEach(() => {
  follow.mockReset();
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

const draw = (markdown: string) =>
  act(() => {
    root.render(
      <LinkContext.Provider value={follow}>
        <Prose className="msg" text={markdown} />
      </LinkContext.Provider>,
    );
  });

const click = (node: Element, init: MouseEventInit = {}) => {
  const event = new MouseEvent("click", { bubbles: true, cancelable: true, ...init });
  act(() => {
    node.dispatchEvent(event);
  });
  return event;
};

describe("Prose", () => {
  it("hands over the href as written, not as the DOM resolves it", async () => {
    await draw("see [the chart](out/plot.csv)");
    const anchor = container.querySelector("a.prose-link");
    expect(anchor).not.toBeNull();
    // The property would read `tauri://localhost/out/plot.csv` — the one form
    // the router cannot make sense of.
    expect((anchor as HTMLAnchorElement).href).not.toBe("out/plot.csv");

    const event = click(anchor!);
    expect(follow).toHaveBeenCalledWith("out/plot.csv", false);
    // Nothing may reach the document's own navigation.
    expect(event.defaultPrevented).toBe(true);
  });

  it("passes the modifier through as the same 'open this as well' it means elsewhere", async () => {
    await draw("[report](out/report.html)");
    click(container.querySelector("a.prose-link")!, { ctrlKey: true });
    expect(follow).toHaveBeenCalledWith("out/report.html", true);
  });

  it("ignores a click on ordinary prose", async () => {
    await draw("just a sentence");
    click(container.querySelector("p")!);
    expect(follow).not.toHaveBeenCalled();
  });

  it("leaves a refused scheme unclickable rather than routing it", async () => {
    // `rich.tsx` strips the affordance from a scheme it will not render as a
    // link; without an anchor there is nothing here to follow, which is the
    // second half of that refusal rather than a new rule.
    await draw("[click me](javascript:alert(1))");
    expect(container.querySelector("a.prose-link")).toBeNull();
    click(container.querySelector(".link-refused") ?? container.querySelector("p")!);
    expect(follow).not.toHaveBeenCalled();
  });

  it("swallows the click when nothing is listening", async () => {
    // The design preview and the tests draw prose with nowhere for a link to
    // go. A dead link there is correct; navigating the app away is not.
    await act(() => {
      root.render(<Prose className="msg" text="[x](https://example.com)" />);
    });
    const event = click(container.querySelector("a.prose-link")!);
    expect(event.defaultPrevented).toBe(true);
  });
});
