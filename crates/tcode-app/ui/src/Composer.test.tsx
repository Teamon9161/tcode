import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  onAttach: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("./Chips", () => ({ Chips: () => null }));

import { Composer } from "./Composer";

// React's `act` warns without this flag, and the warning is noise here.
(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let container: HTMLDivElement;
const onSubmit = vi.fn<(text: string) => void>();
const onChange = vi.fn<(value: string) => void>();

/** Writes the draft the way the window does, bypassing the composer — a rewind
 *  restoring a prompt, the file tree appending a path. */
let publishFromOutside: (value: string) => void = () => {};

function Harness() {
  const [value, setValue] = useState("");
  publishFromOutside = setValue;
  const publish = (next: string) => {
    onChange(next);
    setValue(next);
  };
  return (
    <Composer
      value={value}
      running={false}
      disabled={false}
      attachments={[]}
      meter={{ context: 0, estimated: false, turn: { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0 } }}
      planFirst={false}
      onPlanFirst={() => {}}
      onChange={publish}
      onAttach={mocks.onAttach}
      onDetach={() => {}}
      onSubmit={onSubmit}
      onInterrupt={() => {}}
    />
  );
}

function render() {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  act(() => root.render(<Harness />));
}

function field() {
  return container.querySelector("textarea")!;
}

function type(text: string) {
  const input = field();
  const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, "value")!.set!;
  act(() => {
    setter.call(input, text);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

function keydown(key: string, properties: Record<string, unknown> = {}) {
  const event = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true });
  for (const [name, value] of Object.entries(properties)) {
    Object.defineProperty(event, name, { value });
  }
  act(() => field().dispatchEvent(event));
  return event;
}

async function paste(clipboardData: Pick<DataTransfer, "types" | "files" | "items">) {
  const event = new Event("paste", { bubbles: true, cancelable: true });
  Object.defineProperty(event, "clipboardData", { value: clipboardData });
  await act(async () => {
    field().dispatchEvent(event);
    // Browser file decoding and the native fallback each settle in a microtask.
    for (let index = 0; index < 4; index += 1) await Promise.resolve();
  });
  return event;
}

beforeEach(() => {
  onSubmit.mockReset();
  onChange.mockReset();
  mocks.onAttach.mockReset();
  mocks.invoke.mockReset();
  render();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

describe("the composer input", () => {
  it("leaves Enter to an active IME candidate instead of submitting the draft", () => {
    type("中");
    const input = field();
    input.focus();

    const enter = keydown("Enter", { isComposing: true });

    expect(onSubmit).not.toHaveBeenCalled();
    expect(enter.defaultPrevented).toBe(false);
    expect(field()).toBe(input);
    expect(input.value).toBe("中");
    expect(document.activeElement).toBe(input);
  });

  it("recognizes WebKit's composition key code when its composition flag is late", () => {
    type("中");

    const enter = keydown("Enter", { keyCode: 229 });

    expect(onSubmit).not.toHaveBeenCalled();
    expect(enter.defaultPrevented).toBe(false);
    expect(field().value).toBe("中");
  });

  it("sends a settled draft on an ordinary Enter, carrying the text with it", () => {
    type("send this");

    const enter = keydown("Enter");

    // The text is the argument rather than something the window is asked for
    // afterwards: the draft is published on an idle, so a prompt typed and sent
    // inside that window is not in the window's state yet.
    expect(onSubmit).toHaveBeenCalledExactlyOnceWith("send this");
    expect(enter.defaultPrevented).toBe(true);
    expect(field().value).toBe("");
  });

  it("keeps a keystroke to itself until the typing settles", () => {
    vi.useFakeTimers();
    try {
      type("a");
      type("ab");
      type("abc");

      // Nothing has crossed into the window yet. That is what stops a long
      // conversation from being re-rendered — and an IME preedit from being
      // cancelled — once per character.
      expect(onChange).not.toHaveBeenCalled();
      expect(field().value).toBe("abc");

      act(() => vi.runAllTimers());

      expect(onChange).toHaveBeenCalledExactlyOnceWith("abc");
    } finally {
      vi.useRealTimers();
    }
  });

  it("settles the draft immediately when the field is left", () => {
    vi.useFakeTimers();
    try {
      type("mention this");
      // `focusout`, not `blur`: React delegates `onBlur` to the bubbling one.
      act(() => field().dispatchEvent(new FocusEvent("focusout", { bubbles: true })));

      // Blur lands before the click that follows it, so whatever reads the
      // draft next — the file tree's mention — sees what is on screen.
      expect(onChange).toHaveBeenCalledExactlyOnceWith("mention this");
    } finally {
      vi.useRealTimers();
    }
  });

  it("publishes nothing while an IME composition is open", () => {
    vi.useFakeTimers();
    try {
      act(() => field().dispatchEvent(new CompositionEvent("compositionstart", { bubbles: true })));
      type("ni");
      act(() => vi.runAllTimers());

      // A preedit is the IME's working area, not a draft.
      expect(onChange).not.toHaveBeenCalled();

      const input = field();
      const setter = Object.getOwnPropertyDescriptor(
        window.HTMLTextAreaElement.prototype,
        "value",
      )!.set!;
      act(() => {
        setter.call(input, "你");
        input.dispatchEvent(new CompositionEvent("compositionend", { bubbles: true, data: "你" }));
      });
      act(() => vi.runAllTimers());

      expect(onChange).toHaveBeenCalledExactlyOnceWith("你");
    } finally {
      vi.useRealTimers();
    }
  });

  it("adopts a draft written from outside", () => {
    vi.useFakeTimers();
    try {
      type("half a sentence");
      act(() => vi.runAllTimers());
      onChange.mockClear();

      // A rewind putting a prompt back, or the file tree appending a path.
      act(() => publishFromOutside("half a sentence @src/main.rs "));

      expect(field().value).toBe("half a sentence @src/main.rs ");
      // Adopting is not a change of its own; echoing it back would be a loop.
      expect(onChange).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it("leaves Shift+Enter available for a newline", () => {
    type("first line");

    const enter = keydown("Enter", { shiftKey: true });

    expect(onSubmit).not.toHaveBeenCalled();
    expect(enter.defaultPrevented).toBe(false);
  });

  it("uses the native clipboard when WebKit promises an image without a File", async () => {
    mocks.invoke.mockResolvedValue({ media_type: "image/png", data: "aGVsbG8=" });
    const transfer = {
      types: ["image/png"],
      files: { length: 0, item: () => null },
      items: [{ kind: "file", type: "image/png", getAsFile: () => null }],
    } as unknown as DataTransfer;

    const event = await paste(transfer);

    expect(event.defaultPrevented).toBe(true);
    expect(mocks.invoke).toHaveBeenCalledWith("clipboard_image");
    expect(mocks.onAttach).toHaveBeenCalledWith([
      expect.objectContaining({
        mediaType: "image/png",
        data: "aGVsbG8=",
        url: "data:image/png;base64,aGVsbG8=",
      }),
    ]);
  });

  it("leaves ordinary text paste entirely to the textarea", async () => {
    const transfer = {
      types: ["text/plain"],
      files: { length: 0, item: () => null },
      items: [],
    } as unknown as DataTransfer;

    const event = await paste(transfer);

    expect(event.defaultPrevented).toBe(false);
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.onAttach).not.toHaveBeenCalled();
  });
});
