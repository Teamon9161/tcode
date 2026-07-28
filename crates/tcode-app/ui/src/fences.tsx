import { useState, type ReactNode } from "react";

import { Code } from "./components/Code";
import { Patch } from "./components/Diff";
import { Math } from "./math";
import { Sandbox } from "./Sandbox";
import type { SandboxKind } from "./sandbox/protocol";

/**
 * What a fenced block turns into, keyed by its language tag.
 *
 * This is the same shape as core's three registries (`Tool`, `SlashCommand`,
 * `ToolRenderer`): adding a kind of rich output is a new entry here, and the
 * transcript never learns another language name. A tag with no entry falls
 * through to a highlighted code block, which is why the list can stay short.
 *
 * `view` decides what a block shows *first*, and it is the only thing
 * separating a diagram from a snippet:
 *
 *  - `preview` — the artifact is the point (a chart, a diagram). Source is one
 *    click away.
 *  - `code` — the code is the point. A ```html block is usually a model showing
 *    markup, not asking for it to be run, so rendering it on sight would be
 *    both wrong and the one case where a mistake actually executes something.
 *    Preview stays available; it is just not assumed.
 */
type Fence = {
  languages: string[];
  render(body: string, language: string): ReactNode;
};

const sandbox =
  (kind: SandboxKind, label: string, view: "preview" | "code") =>
  (body: string, language: string) => (
    <Previewable
      source={body}
      language={language}
      label={label}
      kind={kind}
      initial={view}
    />
  );

const FENCES: Fence[] = [
  {
    languages: ["mermaid"],
    render: sandbox("mermaid", "diagram", "preview"),
  },
  {
    languages: ["echarts", "chart"],
    render: sandbox("echarts", "chart", "preview"),
  },
  {
    languages: ["html", "svg"],
    render: sandbox("html", "artifact", "code"),
  },
  {
    languages: ["math", "tex", "latex"],
    render: (body) => <Math tex={body.trim()} display />,
  },
  {
    languages: ["diff", "patch"],
    render: (body) => <Patch text={body} />,
  },
];

const LOOKUP = new Map<string, Fence>();
for (const fence of FENCES) {
  for (const language of fence.languages) LOOKUP.set(language, fence);
}

export function renderFence(body: string, language: string): ReactNode {
  const fence = LOOKUP.get(language.toLowerCase());
  return fence ? fence.render(body, language) : <Code source={body} language={language} />;
}

/** A block that has both a rendered form and a source form, with the toggle
 *  that lets either be the one you wanted. */
function Previewable({
  source,
  language,
  label,
  kind,
  initial,
}: {
  source: string;
  language: string;
  label: string;
  kind: SandboxKind;
  initial: "preview" | "code";
}) {
  const [view, setView] = useState(initial);

  return (
    <div className="previewable">
      <div className="previewable-bar">
        <span className="code-lang">{label}</span>
        <div className="segmented segmented-xs" role="group">
          <button
            className={view === "preview" ? "is-on" : undefined}
            onClick={() => setView("preview")}
            aria-pressed={view === "preview"}
          >
            preview
          </button>
          <button
            className={view === "code" ? "is-on" : undefined}
            onClick={() => setView("code")}
            aria-pressed={view === "code"}
          >
            source
          </button>
        </div>
      </div>
      {view === "preview" ? (
        <Sandbox kind={kind} source={source} label={label} />
      ) : (
        <Code source={source} language={language} />
      )}
    </div>
  );
}
