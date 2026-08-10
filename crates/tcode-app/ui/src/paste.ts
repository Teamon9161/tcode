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
  return uniquePasted(await Promise.all(imageFiles(transfer).map(read)));
}

/** Keep one decoded image once per incoming batch as well as across batches.
 * Chromium may hand the same clipboard item out as distinct `File`s whose
 * metadata does not agree, but their bytes do. */
export function uniquePasted(items: Pasted[], seen = new Set<string>()): Pasted[] {
  return items.filter((item) => {
    const key = `${item.mediaType}\u0000${item.data}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

/**
 * Chromium exposes an image through `files`, `items`, or both. Read both
 * views and preserve each file once.
 *
 * "Each file once" is a structural test, not an identity one: engines can hand
 * the same paste out as two `File` objects, one from `files` and one from
 * `getAsFile()`.
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
  return imageFiles(transfer).length > 0;
}

async function read(file: File): Promise<Pasted> {
  const url = await asDataUrl(file);
  const shrunk = await shrink(url, file.type);
  return pasted(
    shrunk.mediaType,
    shrunk.url.slice(shrunk.url.indexOf(",") + 1),
    file.name || undefined,
  );
}

function pasted(mediaType: string, data: string, name?: string): Pasted {
  counter += 1;
  return {
    id: `paste-${counter}`,
    mediaType,
    data,
    url: `data:${mediaType};base64,${data}`,
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
): Promise<{ url: string; mediaType: string }> {
  const image = await load(url).catch(() => null);
  if (!image) return { url, mediaType };
  const longest = Math.max(image.width, image.height);
  if (longest <= MAX_EDGE) return { url, mediaType };

  const scale = MAX_EDGE / longest;
  const width = Math.round(image.width * scale);
  const height = Math.round(image.height * scale);
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  const context = canvas.getContext("2d");
  if (!context) return { url, mediaType };
  context.drawImage(image, 0, 0, width, height);
  // JPEG for photographs and screenshots alike: a resampled screenshot has no
  // flat regions left for PNG to win on, and the size difference is large.
  return {
    url: canvas.toDataURL("image/jpeg", 0.85),
    mediaType: "image/jpeg",
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
