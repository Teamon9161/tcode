// The app webview's entire view of the process it lives in.
//
// Two functions. Not `ipcRenderer`, not a channel name, not an object with a
// `send` on it — because whatever is reachable from here is reachable from any
// script that ends up running in this document, and the rules that keep model
// output from becoming markup (AGENTS.md rule 10) are written against exactly
// this surface being small enough to state in a sentence.
//
// The counterpart in the frontend is `ui/src/ipc.ts`; the `Bridge` type there
// is this file, and the two must be changed together.
//
// CommonJS on purpose: a sandboxed preload is not an ES module, and `sandbox:
// true` is not negotiable for the document that talks to the backend.

const { contextBridge, ipcRenderer } = require("electron");

/** Event name -> the listeners waiting on it. */
const listeners = new Map();

// One `ipcRenderer.on` for every event in the app, rather than one channel per
// event name: the fan-out is a Map lookup here instead of a subscription in the
// main process, and — the part that matters — it means adding an event to
// `bridge.rs` never requires touching this file.
ipcRenderer.on("tcode:event", (_event, name, payload) => {
  for (const deliver of listeners.get(name) ?? []) deliver(payload);
});

contextBridge.exposeInMainWorld("tcode", {
  /**
   * Call a command. `main.js` answers the ones about this window and forwards
   * everything else down the pipe to the sidecar.
   *
   * The reply is an envelope rather than a thrown error, and it is unwrapped
   * here so a failed command rejects with **the backend's own string** —
   * `ipcMain.handle` would otherwise wrap it in "Error invoking remote method
   * 'tcode:invoke'", and that string is shown to people (rule 7).
   */
  async invoke(method, args) {
    const reply = await ipcRenderer.invoke("tcode:invoke", method, args ?? {});
    if (reply.error !== undefined) throw reply.error;
    return reply.ok;
  },

  /** Subscribe. Returns the unsubscribe, synchronously — there is no round
   *  trip to make, and `ipc.ts` wraps it back into the promise its callers
   *  already unsubscribe through. */
  listen(name, deliver) {
    const known = listeners.get(name) ?? new Set();
    known.add(deliver);
    listeners.set(name, known);
    return () => known.delete(deliver);
  },
});
