import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { invoke } from "@ipc";

import { useSession } from "./session";

/**
 * A file loaded as a page, from the app's own loopback origin.
 *
 * ## Why this is not the sandbox frame
 *
 * `Sandbox.tsx` renders markup the *model* wrote, and its frame is deliberately
 * an opaque origin: no `allow-same-origin`, so `parent.__TAURI__` and everything
 * near it throws. That is exactly right for a string of HTML arriving inside a
 * model's reply, and exactly wrong for a report a script produced, because a
 * report is a document with parts. It runs a script to draw itself; it asks for
 * `./fig1.png`; it fetches its own data. An opaque origin cannot do any of
 * those, and `innerHTML` never runs `<script>` in any origin — so that path
 * renders a plotly file as a blank div and always would have.
 *
 * So this frame gets its own origin instead of no origin. The app is served
 * from `tauri://localhost` (`http://tauri.localhost` on Windows) and this from
 * `http://127.0.0.1:<port>`; they are different origins, so the same-origin
 * policy — not an attribute, not a parser — is what keeps the page away from
 * the app's IPC. **That is a stronger boundary than the sandbox one, because it
 * holds even if every attribute below were dropped.**
 *
 * The attributes are still there, and each is a separate decision:
 *
 *  - `allow-scripts` and `allow-same-origin`: the point of the exercise. Paired,
 *    they are only a warning sign when a frame is same-origin *with the parent*,
 *    which this one can never be — it keeps its own 127.0.0.1 origin, and that
 *    origin is what `fetch` and `localStorage` inside the report resolve
 *    against.
 *  - `allow-forms` and `allow-downloads`: a report with a filter control, a
 *    chart with a "save as PNG" button.
 *  - **No `allow-top-navigation`**, which is the load-bearing omission: without
 *    it a shown file cannot navigate the window it is embedded in. A report that
 *    could replace the app with a page of its choosing would make "look at this
 *    file" a much bigger sentence than it reads as.
 *  - No `allow-modals` and no `allow-popups`: an `alert` from a report would
 *    block the window it is embedded in, and nothing here has a reason to open
 *    another one. Both fail by doing nothing, which is the right direction.
 *
 * Height cannot be measured across an origin, so unlike `Sandbox` there is no
 * message protocol to ask for it — the frame fills what CSS gives it. In a pane
 * that is the pane. Inline it is a *fitted* band, and that is the one piece of
 * geometry here worth explaining.
 *
 * A report is authored for a desktop window: a plotly figure defaults to
 * 700×450, a dashboard is laid out at a thousand pixels or more. The reading
 * column is `--measure`, and inline this used to be that width by a flat
 * `20rem` — so a chart arrived clipped on both axes, with the frame's own
 * scrollbars as the only way through it and the report's whole shape (which is
 * the thing a chart *is*) never on screen at once.
 *
 * Neither side of that can be fixed by asking: the page cannot be measured, and
 * its bytes must not be parsed on this side (see 11b). What is left is to
 * choose the viewport rather than inherit it. The frame is laid out at
 * `FIT_VIEWPORT` logical pixels — the width these documents were written for —
 * and scaled by CSS to whatever the column happens to be. The page believes it
 * is on a desktop, and the reader sees all of that desktop's width. Vertical
 * overflow still scrolls inside the frame, because a report can be any length
 * and a band that grew to fit one would stop being a band.
 *
 * The cost is honest and bounded: text at roughly 0.7×, which is a thumbnail
 * that shows the whole thing rather than a full-size window onto a corner of
 * it. Full size is one click away and always was — the pop-out on the row above
 * puts the same component in a pane at scale 1.
 */
/**
 * The frame's capabilities, as one string so a test can hold them still.
 *
 * Spelled out here rather than inline in the JSX because the interesting half
 * of this list is what it does *not* contain, and an absence is not something a
 * rendered-output assertion can reach when the frame has not resolved its URL
 * yet. `FileBody.test.tsx` pins the omissions against this constant.
 */
export const FRAME_SANDBOX = "allow-scripts allow-same-origin allow-forms allow-downloads";

/** The width an inline report is laid out at, whatever the column is.
 *
 *  A guess, and the only one available — the document cannot be asked. It is
 *  the width these files are written for: matplotlib and plotly save around
 *  700–1000 CSS px, quarto and notebook exports set a container near 1000, and
 *  a responsive page given 1000 lays out as the desktop page it was checked
 *  against rather than collapsing to its phone stacking. */
export const FIT_VIEWPORT = 1000;

/** The band's shape. 16:10 rather than the 4:3 a chart tends to be, because
 *  the same band also holds documents, and for those every row of the ratio is
 *  another paragraph before the scroll starts. */
export const FIT_ASPECT = 0.625;

/**
 * The inline frame's geometry for a column `width` wide.
 *
 * Pure, exported and tested, because the two numbers have to agree: the wrapper
 * reserves `band` and the frame paints `logical × logical * FIT_ASPECT` scaled
 * by `scale`, and a disagreement is a strip of the report cut off or a strip of
 * empty page under it.
 *
 * A column wider than the viewport is not magnified — `logical` grows instead,
 * so the report gets the extra room as room. Scaling *up* would be the one
 * result nobody asked for: a blurry page that fits just as well at 1:1.
 */
export function fit(width: number): { logical: number; scale: number; band: number } {
  const logical = Math.max(FIT_VIEWPORT, width);
  const scale = width > 0 ? width / logical : 1;
  return { logical, scale, band: Math.round(logical * FIT_ASPECT * scale) };
}

export function Framed({
  path,
  label,
  revision,
  inline = false,
}: {
  path: string;
  label: string;
  /**
   * Bumped by whoever knows the file changed — the reload button, a save.
   *
   * It is the frame's React key, not a query parameter on its URL. Nothing here
   * can reach across the origin to tell the document to reload, so the handle
   * has to be the element itself: a new key discards the frame and mounts a
   * fresh one, which re-requests the URL, which `no-store` makes a real read.
   * A cache-busting parameter would do the same job for the real server and
   * break for any URL that does not take one — a `blob:` in the design preview
   * being exactly that, which is how this was found.
   */
  revision?: string | number;
  inline?: boolean;
}) {
  const session = useSession();
  const [url, setUrl] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const box = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);

  // Measured rather than read from a media query: the column is a pane's share
  // of the window, so it moves with every divider drag and every split, and no
  // breakpoint knows what it currently is. A layout effect so the first paint
  // is already at the right scale — the alternative flashes a 1000px page in a
  // 700px box. Bounded work: this fires on mount and on resize, never per
  // render (`WebPane`'s note on why that distinction matters here).
  useLayoutEffect(() => {
    const node = box.current;
    if (!node) return;
    const measure = () => setWidth(node.getBoundingClientRect().width);
    measure();
    const watch = new ResizeObserver(measure);
    watch.observe(node);
    return () => watch.disconnect();
  }, [inline]);

  useEffect(() => {
    let live = true;
    setUrl(null);
    setFailure(null);
    invoke<string>("serve_url", { session, path })
      .then((served) => live && setUrl(served))
      .catch((error) => live && setFailure(String(error)));
    return () => {
      live = false;
    };
  }, [session, path]);

  const { logical, scale, band } = fit(width);

  // One `<iframe>` in this file, in both framings. The alternative — a branch
  // per framing, each with its own element — is how the two frames in this app
  // swap attributes (11b): the capability list would appear twice, and a change
  // to one copy is invisible in review. `boundary.test.ts` reads this file for
  // exactly that.
  const body = failure ? (
    <p className="inspect-empty">{failure}</p>
  ) : url ? (
    <iframe
      key={revision}
      className={`framed${inline ? " is-inline" : ""}`}
      src={url}
      // Cross-origin already; see the note above for why each of these is here
      // and why top-level navigation is not.
      sandbox={FRAME_SANDBOX}
      title={label}
      // Inline, the page is laid out at the viewport it was written for and
      // scaled to the column. In a pane it takes the pane, at 1:1.
      style={
        inline
          ? {
              width: logical,
              height: Math.round(logical * FIT_ASPECT),
              transform: `scale(${scale})`,
            }
          : undefined
      }
    />
  ) : (
    <p className="inspect-empty">loading…</p>
  );

  if (!inline) return body;

  // The wrapper is rendered before there is anything to put in it, and that is
  // deliberate: it is what gets measured, and it holds the band's height from
  // the first frame, so a report arriving does not shove the rest of the
  // conversation down the screen.
  return (
    <div className="framed-fit" ref={box} style={{ height: band }}>
      {body}
    </div>
  );
}
