const assert = require("node:assert/strict");
const test = require("node:test");

const { browserVerbs } = require("./browser");

class FakeWebContentsView {
  static instances = [];

  constructor(options) {
    this.options = options;
    this.operations = [];
    this.bounds = [];
    this.webContents = {
      on() {},
      setWindowOpenHandler() {},
      loadURL: async () => {},
      close() {},
      getURL: () => "about:blank",
      debugger: {
        isAttached: () => false,
      },
    };
    FakeWebContentsView.instances.push(this);
  }

  setVisible(visible) {
    this.operations.push(["visible", visible]);
  }

  setBounds(bounds) {
    this.bounds.push(bounds);
    this.operations.push(["bounds", bounds]);
  }
}

function harness() {
  FakeWebContentsView.instances = [];
  const partition = {
    setPermissionRequestHandler() {},
  };
  const verbs = browserVerbs(
    {
      window: {
        contentView: {
          addChildView() {},
          removeChildView() {},
        },
      },
      emit() {},
      resolveUrl: async (url) => url,
    },
    {
      WebContentsView: FakeWebContentsView,
      session: { fromPartition: () => partition },
    },
  );
  const first = verbs.browser_open({
    rect: { x: 0, y: 0, width: 800, height: 600 },
    select: true,
  });
  return { verbs, first, view: FakeWebContentsView.instances[0] };
}

function expectShownAt(view, rect) {
  assert.deepEqual(view.operations.slice(-2), [
    ["visible", true],
    ["bounds", rect],
  ]);
}

test("a hidden browser defers divider bounds until it is shown", () => {
  const { verbs, view } = harness();
  verbs.browser_visible({ visible: false });
  view.bounds.length = 0;
  view.operations.length = 0;

  verbs.browser_bounds({ rect: { x: 100, y: 20, width: 600, height: 500 } });
  verbs.browser_bounds({ rect: { x: 140, y: 20, width: 560, height: 500 } });

  assert.equal(view.bounds.length, 0, "hidden pages must not re-layout on drag frames");

  verbs.browser_visible({ visible: true });
  assert.deepEqual(view.bounds, [{ x: 140, y: 20, width: 560, height: 500 }]);
  expectShownAt(view, { x: 140, y: 20, width: 560, height: 500 });
});

test("a visible browser still follows an ordinary bounds update", () => {
  const { verbs, view } = harness();
  verbs.browser_visible({ visible: true });
  view.bounds.length = 0;

  verbs.browser_bounds({ rect: { x: 80, y: 30, width: 720, height: 540 } });

  assert.deepEqual(view.bounds, [{ x: 80, y: 30, width: 720, height: 540 }]);
});

test("showing an existing browser places the current tab after visibility", () => {
  const { verbs, view } = harness();
  const rect = { x: 40, y: 70, width: 900, height: 640 };
  view.operations.length = 0;

  verbs.browser_show({ rect });

  expectShownAt(view, rect);
});

test("selecting a tab places it after it becomes visible", () => {
  const { verbs } = harness();
  verbs.browser_visible({ visible: true });
  const id = verbs.browser_open({ select: false });
  const selected = FakeWebContentsView.instances[1];
  selected.operations.length = 0;

  verbs.browser_select({ id });

  expectShownAt(selected, { x: 0, y: 0, width: 800, height: 600 });
});

test("opening a selected tab in a shown browser places it after visibility", () => {
  const { verbs } = harness();
  verbs.browser_visible({ visible: true });
  const rect = { x: 25, y: 45, width: 760, height: 520 };

  verbs.browser_open({ rect, select: true });

  expectShownAt(FakeWebContentsView.instances[1], rect);
});

test("closing a background tab re-places the visible current tab", () => {
  const { verbs, view } = harness();
  verbs.browser_visible({ visible: true });
  const background = verbs.browser_open({ select: false });
  view.operations.length = 0;

  verbs.browser_close({ id: background });

  expectShownAt(view, { x: 0, y: 0, width: 800, height: 600 });
});
