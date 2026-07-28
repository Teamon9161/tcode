import * as echarts from "echarts";

import { register } from "./runtime";

/** Charts. Loaded only when a conversation actually produces one. */
register("echarts", (stage, source, token) => {
  const option = JSON.parse(source) as Record<string, unknown>;

  const box = document.createElement("div");
  // A chart has no intrinsic height, so one is chosen here rather than left to
  // collapse. The model may override it through the spec's own `height`.
  const height = typeof option.height === "number" ? option.height : 320;
  box.style.width = "100%";
  box.style.height = `${height}px`;
  stage.replaceChildren(box);

  const chart = echarts.init(box, null, { renderer: "canvas" });
  chart.setOption({
    backgroundColor: "transparent",
    textStyle: { fontFamily: token("--font-ui"), color: token("--body") },
    // A chart is the one place chroma encodes data rather than state, so it may
    // carry a series palette — but it has to be *this* palette. echarts' own
    // default blue is a second visual system arriving uninvited. Brand first, so
    // the common single-series chart is drawn in the app's own ink.
    color: [
      token("--brand"),
      token("--ink"),
      token("--amber"),
      token("--danger"),
      token("--muted"),
    ],
    ...option,
    // After the spread, not before: axes are merged field by field so a spec
    // that sets its own `xAxis` keeps every option it named and still inherits
    // the treatment it did not.
    xAxis: axis(option.xAxis, token),
    yAxis: axis(option.yAxis, token),
  });
  new ResizeObserver(() => chart.resize()).observe(box);
});

/** Axes recede: they are scaffolding, and the series is the content. */
function axis(given: unknown, token: (name: string) => string) {
  const styled = {
    axisLine: { lineStyle: { color: token("--line") } },
    axisTick: { lineStyle: { color: token("--line") } },
    axisLabel: { color: token("--muted") },
    nameTextStyle: { color: token("--faint") },
    splitLine: { lineStyle: { color: token("--line") } },
  };
  if (Array.isArray(given)) return given.map((one) => ({ ...styled, ...(one as object) }));
  if (given && typeof given === "object") return { ...styled, ...(given as object) };
  return styled;
}
