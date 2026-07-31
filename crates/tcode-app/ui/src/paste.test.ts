import { describe, expect, it } from "vitest";

import { imageFiles, isImagePaste } from "./paste";

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
});
