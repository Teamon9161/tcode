import { Component, type ErrorInfo, type ReactNode } from "react";

import { Mark } from "./components/Mark";

/**
 * The last thing between a thrown error and an empty window.
 *
 * Rule 7 says no promise here may reject silently, and `App`'s `Fault` screen is
 * how a failed startup says so. Both were only ever half the guarantee: a
 * rejected promise had somewhere to go, and an error thrown *during render* had
 * nowhere. React's answer to an uncaught render error is to unmount the root —
 * so the failure mode rule 7 exists to prevent was still reachable by another
 * road, and arrived looking exactly like a hang: a white window, no controls,
 * nothing to report.
 *
 * This is not a way to keep running through errors. It is a way to be *told*.
 * The window is already lost when this renders; what it buys is the difference
 * between "it broke" and a message naming what broke, which is the difference
 * between a bug report and a shrug.
 *
 * Deliberately narrow: it wraps the whole app, not individual panes. A pane that
 * swallowed its own errors would keep a broken conversation on screen looking
 * fine, which is the thing this app refuses to do everywhere else.
 */
export class Boundary extends Component<{ children: ReactNode }, { failure: Error | null }> {
  state: { failure: Error | null } = { failure: null };

  static getDerivedStateFromError(failure: Error) {
    return { failure };
  }

  componentDidCatch(failure: Error, info: ErrorInfo) {
    // Also to the console, where a devtools session can see the component stack
    // that the screen below deliberately does not show a user.
    console.error("tcode: unhandled render error", failure, info.componentStack);
  }

  render() {
    const { failure } = this.state;
    if (!failure) return this.props.children;

    return (
      <div className="fault">
        <Mark size={22} state="failed" />
        <h1>tcode hit an error and stopped drawing</h1>
        <p>{failure.message || String(failure)}</p>
        {failure.stack && <pre className="fault-stack">{failure.stack}</pre>}
        <p className="fault-hint">
          Reloading starts the window again; the conversations themselves are on
          disk and are not affected.
        </p>
        <button className="btn" onClick={() => window.location.reload()}>
          reload
        </button>
      </div>
    );
  }
}
