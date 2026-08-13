import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  init: vi.fn(),
  fromBytes: vi.fn(),
  ironCalc: vi.fn(),
}));

vi.mock("@ipc", () => ({ invoke: mocks.invoke }));
vi.mock("@ironcalc/workbook", () => ({
  init: mocks.init,
  Model: { from_bytes: mocks.fromBytes },
  IronCalc: (props: { canEdit: boolean }) => {
    mocks.ironCalc(props);
    return <div data-ironcalc data-can-edit={String(props.canEdit)} />;
  },
}));
vi.mock("@ironcalc/workbook/style.css", () => ({}));

import { SessionContext } from "./session";
import { SpreadsheetView } from "./SpreadsheetView";

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

let root: Root;
let container: HTMLDivElement;

async function draw(path: string) {
  await act(async () => {
    root.render(
      <SessionContext.Provider value="s">
        <SpreadsheetView path={path} />
      </SessionContext.Provider>,
    );
  });
}

async function flush() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe("SpreadsheetView", () => {
  beforeEach(() => {
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    mocks.invoke.mockReset();
    mocks.init.mockReset();
    mocks.fromBytes.mockReset();
    mocks.ironCalc.mockReset();
    mocks.init.mockResolvedValue(undefined);
    mocks.invoke.mockResolvedValue("AAE=");
    mocks.fromBytes.mockReturnValue({ free: vi.fn(), toBytes: vi.fn(() => new Uint8Array()) });
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it("loads legacy .xls files through IronCalc as read-only previews", async () => {
    await draw("real_trades/交易所补单_0720.xls");
    await flush();

    expect(mocks.init).toHaveBeenCalledOnce();
    expect(mocks.invoke).toHaveBeenCalledWith("spreadsheet_load", {
      session: "s",
      path: "real_trades/交易所补单_0720.xls",
    });
    expect(mocks.fromBytes).toHaveBeenCalled();
    expect(mocks.ironCalc).toHaveBeenCalledWith(expect.objectContaining({ canEdit: false }));
    expect(container.textContent).toContain("converted for spreadsheet preview");
  });

  it("loads csv files through IronCalc as read-only previews", async () => {
    await draw("data/trades.csv");
    await flush();

    expect(mocks.invoke).toHaveBeenCalledWith("spreadsheet_load", {
      session: "s",
      path: "data/trades.csv",
    });
    expect(mocks.ironCalc).toHaveBeenCalledWith(expect.objectContaining({ canEdit: false }));
    expect(container.textContent).toContain("converted for spreadsheet preview");
  });

  it("keeps .xlsx files editable", async () => {
    await draw("book.xlsx");
    await flush();

    expect(mocks.invoke).toHaveBeenCalledWith("spreadsheet_load", {
      session: "s",
      path: "book.xlsx",
    });
    expect(mocks.ironCalc).toHaveBeenCalledWith(expect.objectContaining({ canEdit: true }));
    expect(container.textContent).not.toContain("read-only");
  });
});
