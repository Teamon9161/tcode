/**
 * Images on their way into a prompt.
 *
 * Pasting a screenshot is how a person says "this, right here" about something
 * they cannot name — a broken layout, a stack trace in another window, a
 * whiteboard. A desktop app that cannot take one is asking the user to describe
 * a picture in words.
 *
 * The clipboard hands over a `File`; the wire wants base64. In between, one
 * decision worth stating: oversized images are **resized rather than rejected**.
 * A 4K screenshot is the single most common thing anybody pastes, it is also
 * routinely over the provider's per-image limit, and "that image is too large"
 * is a dead end for the one input the user actually has. The long edge caps at
 * `MAX_EDGE`, which is the resolution the models sample at anyway, so nothing
 * legible is lost and the tokens are not spent.
 */

export type Pasted = {
  /** Stable while the draft is being written, so a chip can be removed. */
  id: string;
  /** `image/png`, `image/jpeg`, … — what the provider is told it is. */
  mediaType: string;
  /** Base64 payload, no `data:` prefix: that is the shape `ContentBlock::Image` takes. */
  data: string;
  /** For the thumbnail. Needs `img-src data:` in the CSP. */
  url: string;
  /** The dimensions the provider will receive. The native clipboard path
   *  encodes to PNG while the DOM path resamples to JPEG, so byte comparison
   *  cannot recognise the same paste arriving by both routes — dimensions can
   *  (both sides normalize to the same long edge; see `MAX_EDGE`). */
  width: number;
  height: number;
  name: string;
};

/** Above this, resampling is cheaper than the tokens — and the provider's own
 *  limit is near here too. */
const MAX_EDGE = 1568;

let counter = 0;

/**
 * Every image in a paste or a drop. Non-image entries are ignored: text pastes
 * are the textarea's own business and must keep landing in it.
 */
export async function imagesFrom(transfer: DataTransfer | null): Promise<Pasted[]> {
  return Promise.all(imageFiles(transfer).map(read));
}

/** The native command's narrow response when WebKitGTK has no DOM `File`. */
export type NativeClipboardImage = {
  media_type: string;
  data: string;
  width: number;
  height: number;
};

/** Turn a normalized native clipboard response into the same draft chip as a DOM File. */
export function imageFromNativeClipboard(image: NativeClipboardImage): Pasted {
  if (!image.media_type.startsWith("image/") || image.data.length === 0) {
    throw new Error("the system clipboard returned an invalid image");
  }
  return pasted(image.media_type, image.data, image.width, image.height);
}

/**
 * WebKit can expose a pasted screenshot through `items` without populating
 * `files`, while drag-and-drop commonly supplies both. Read both paths and
 * preserve each file once so either browser shape reaches the prompt.
 *
 * "Each file once" is a structural test, not an identity one: engines have
 * been seen to hand the same paste out as two `File` objects (one from
 * `files`, one from `getAsFile()`), and a chip for each is the "pasted twice"
 * bug a user reports as a duplicate image in the prompt.
 */
export function imageFiles(
  transfer: Pick<DataTransfer, "files" | "items"> | null,
): File[] {
  if (!transfer) return [];
  const files: File[] = [];
  const seen = new Set<string>();
  const consider = (file: File | null) => {
    if (!file || !file.type.startsWith("image/")) return;
    const key = `${file.name}\u0000${file.size}\u0000${file.lastModified}`;
    if (seen.has(key)) return;
    seen.add(key);
    files.push(file);
  };
  for (const file of Array.from(transfer.files)) consider(file);
  for (const item of Array.from(transfer.items)) {
    if (item.kind !== "file") continue;
    consider(item.getAsFile());
  }
  return files;
}

export function isImagePaste(
  transfer: Pick<DataTransfer, "files" | "items"> | null,
): boolean {
  if (!transfer) return false;
  return (
    Array.from(transfer.files).some((file) => file.type.startsWith("image/")) ||
    Array.from(transfer.items).some(
      (item) => item.kind === "file" && item.type.startsWith("image/"),
    )
  );
}

/** A shape remembered from a DOM-path paste, for the native fallback to
 *  compare against (`Composer.tsx`). */
export type RecentPasteShape = { width: number; height: number; at: number };

/**
 * Whether a native clipboard image is a second delivery of a paste the DOM
 * path already attached.
 *
 * One Ctrl+V can reach the composer twice on some platforms: the first event
 * carries a DOM `File`, the second is empty and falls through to the native
 * clipboard. The two reads of the same paste encode differently (the DOM path
 * resamples to JPEG, `normalize_rgba` emits PNG), so bytes cannot be compared;
 * both normalize to the same long edge, so the shape can. A fresh paste of a
 * same-sized image is a deliberate act and is only confused with the previous
 * one if it lands inside the window.
 */
export function matchesRecentPaste(
  recent: ReadonlyArray<RecentPasteShape>,
  width: number,
  height: number,
  now: number,
  windowMs: number,
): boolean {
  return recent.some(
    (entry) => now - entry.at < windowMs && entry.width === width && entry.height === height,
  );
}

/**
 * Whether this paste needs the image path rather than the textarea's native
 * text path. Some WebKitGTK builds provide only an image MIME type, or no DOM
 * clipboard entries at all, for a screenshot; either shape needs a native read.
 */
export function needsNativeImageRead(
  transfer: Pick<DataTransfer, "types" | "files" | "items"> | null,
): boolean {
  if (!transfer) return false;
  const types = Array.from(transfer.types);
  const hasText = types.some((type) => type === "text/plain" || type === "text/html");
  return (
    isImagePaste(transfer) ||
    types.some((type) => type.startsWith("image/")) ||
    (!hasText && types.length === 0 && transfer.files.length === 0 && transfer.items.length === 0)
  );
}

async function read(file: File): Promise<Pasted> {
  const url = await asDataUrl(file);
  const shrunk = await shrink(url, file.type);
  return pasted(
    shrunk.mediaType,
    shrunk.url.slice(shrunk.url.indexOf(",") + 1),
    shrunk.width,
    shrunk.height,
    file.name || undefined,
  );
}

function pasted(mediaType: string, data: string, width: number, height: number, name?: string): Pasted {
  counter += 1;
  return {
    id: `paste-${counter}`,
    mediaType,
    data,
    url: `data:${mediaType};base64,${data}`,
    width,
    height,
    name: name || `pasted image ${counter}`,
  };
}

function asDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error ?? new Error("could not read the image"));
    reader.readAsDataURL(file);
  });
}

/** Scale the long edge down to `MAX_EDGE`, or hand back what came in. */
async function shrink(
  url: string,
  mediaType: string,
): Promise<{ url: string; mediaType: string; width: number; height: number }> {
  const image = await load(url).catch(() => null);
  if (!image) return { url, mediaType, width: 0, height: 0 };
  const longest = Math.max(image.width, image.height);
  if (longest <= MAX_EDGE) return { url, mediaType, width: image.width, height: image.height };

  const scale = MAX_EDGE / longest;
  const width = Math.round(image.width * scale);
  const height = Math.round(image.height * scale);
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  const context = canvas.getContext("2d");
  if (!context) return { url, mediaType, width: image.width, height: image.height };
  context.drawImage(image, 0, 0, width, height);
  // JPEG for photographs and screenshots alike: a resampled screenshot has no
  // flat regions left for PNG to win on, and the size difference is large.
  return {
    url: canvas.toDataURL("image/jpeg", 0.85),
    mediaType: "image/jpeg",
    width,
    height,
  };
}

function load(url: string): Promise<HTMLImageElement> {
  return new Promise((resolve, reject) => {
    const image = new Image();
    image.onload = () => resolve(image);
    image.onerror = () => reject(new Error("not a decodable image"));
    image.src = url;
  });
}
