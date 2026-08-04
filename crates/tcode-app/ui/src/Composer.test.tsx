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
const onSubmit = vi.fn<() => void>();

function Harness() {
  const [value, setValue] = useState("");
  return (
    <Composer
      value={value}
      running={false}
      disabled={false}
      attachments={[]}
      meter={{ context: 0, estimated: false, turn: { input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_write_tokens: 0 } }}
      planFirst={false}
      onPlanFirst={() => {}}
      onChange={setValue}
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

  it("sends a settled draft on an ordinary Enter", () => {
    type("send this");

    const enter = keydown("Enter");

    expect(onSubmit).toHaveBeenCalledOnce();
    expect(enter.defaultPrevented).toBe(true);
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
