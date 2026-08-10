import { describe, expect, it } from "vitest";

import {
  imageFiles,
  isImagePaste,
  matchesRecentPaste,
  needsNativeImageRead,
} from "./paste";

describe("imageFiles", () => {
  it("reads an image exposed only through clipboard items", () => {
    const image = new File(["image"], "clipboard.png", { type: "image/png" });
    const transfer = {
      files: { length: 0, item: () => null },
      items: [
        {
          kind: "file",
          type: "image/png",
          getAsFile: () => image,
        },
      ],
    } as unknown as DataTransfer;

    expect(isImagePaste(transfer)).toBe(true);
    expect(imageFiles(transfer)).toEqual([image]);
  });

  it("does not duplicate an image exposed through files and items", () => {
    const image = new File(["image"], "clipboard.png", { type: "image/png" });
    const transfer = {
      files: { 0: image, length: 1, item: () => image },
      items: [
        {
          kind: "file",
          type: "image/png",
          getAsFile: () => image,
        },
      ],
    } as unknown as DataTransfer;

    expect(imageFiles(transfer)).toEqual([image]);
  });

  it("does not duplicate an image whose files and items are distinct File objects", () => {
    // Some engines hand the same paste out as two File objects — one from
    // `files`, one from `getAsFile()`. Identity comparison misses it, and a
    // chip for each is the "pasted twice" bug.
    const fromFiles = new File(["image"], "clipboard.png", { type: "image/png" });
    const fromItems = new File(["image"], "clipboard.png", { type: "image/png" });
    const transfer = {
      files: { 0: fromFiles, length: 1, item: () => fromFiles },
      items: [
        {
          kind: "file",
          type: "image/png",
          getAsFile: () => fromItems,
        },
      ],
    } as unknown as DataTransfer;

    const files = imageFiles(transfer);
    expect(files).toEqual([fromFiles]);
    expect(files).toHaveLength(1);
  });

  it("recognizes an image item even when WebKit cannot materialize its File", () => {
    const transfer = {
      types: ["image/png"],
      files: { length: 0, item: () => null },
      items: [
        {
          kind: "file",
          type: "image/png",
          getAsFile: () => null,
        },
      ],
    } as unknown as DataTransfer;

    expect(isImagePaste(transfer)).toBe(true);
    expect(imageFiles(transfer)).toEqual([]);
    expect(needsNativeImageRead(transfer)).toBe(true);
  });

  it("does not intercept an ordinary text paste", () => {
    const transfer = {
      types: ["text/plain"],
      files: { length: 0, item: () => null },
      items: [],
    } as unknown as DataTransfer;

    expect(needsNativeImageRead(transfer)).toBe(false);
  });
});

describe("matchesRecentPaste", () => {
  const recent = [
    { width: 1568, height: 784, at: 1_000 },
    { width: 400, height: 300, at: 2_000 },
  ];

  it("matches a native image shaped like a paste the DOM path just attached", () => {
    expect(matchesRecentPaste(recent, 1568, 784, 2_100, 1_500)).toBe(true);
    expect(matchesRecentPaste(recent, 400, 300, 2_100, 1_500)).toBe(true);
  });

  it("ignores a different shape and a paste outside the window", () => {
    expect(matchesRecentPaste(recent, 100, 100, 2_100, 1_500)).toBe(false);
    expect(matchesRecentPaste(recent, 1568, 784, 3_000, 1_500)).toBe(false);
  });
});
