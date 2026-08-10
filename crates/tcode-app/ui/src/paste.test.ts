import { describe, expect, it } from "vitest";

import { imageFiles, isImagePaste, uniquePasted, type Pasted } from "./paste";

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

  it("does not duplicate distinct File objects for the same clipboard item", () => {
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

  it("does not intercept an image MIME type without a DOM File", () => {
    const transfer = {
      files: { length: 0, item: () => null },
      items: [
        {
          kind: "file",
          type: "image/png",
          getAsFile: () => null,
        },
      ],
    } as unknown as DataTransfer;

    expect(imageFiles(transfer)).toEqual([]);
    expect(isImagePaste(transfer)).toBe(false);
  });

  it("does not intercept an ordinary text paste", () => {
    const transfer = {
      files: { length: 0, item: () => null },
      items: [],
    } as unknown as DataTransfer;

    expect(isImagePaste(transfer)).toBe(false);
  });
});


describe("uniquePasted", () => {
  const image = (data: string, id: string): Pasted => ({
    id,
    mediaType: "image/png",
    data,
    url: `data:image/png;base64,${data}`,
    name: "clipboard.png",
  });

  it("keeps one decoded image when a single paste exposes it twice", () => {
    const once = image("same-bytes", "paste-1");
    const duplicate = image("same-bytes", "paste-2");

    expect(uniquePasted([once, duplicate])).toEqual([once]);
  });

  it("preserves genuinely different images", () => {
    const first = image("first", "paste-1");
    const second = image("second", "paste-2");

    expect(uniquePasted([first, second])).toEqual([first, second]);
  });
});
