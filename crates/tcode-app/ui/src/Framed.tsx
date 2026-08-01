import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

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
 * that is the pane; inline it is a fixed band. A report is a page, and a page
 * takes the room it is given rather than reporting an intrinsic size.
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

  if (failure) return <p className="inspect-empty">{failure}</p>;
  if (!url) return <p className="inspect-empty">loading…</p>;

  return (
    <iframe
      key={revision}
      className={`framed${inline ? " is-inline" : ""}`}
      src={url}
      // Cross-origin already; see the note above for why each of these is here
      // and why top-level navigation is not.
      sandbox={FRAME_SANDBOX}
      title={label}
    />
  );
}
