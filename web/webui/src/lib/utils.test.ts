import { describe, expect, it } from "vitest";

import { cn } from "./utils";

describe("cn", () => {
  it("merges conditional classes and resolves Tailwind conflicts", () => {
    expect(cn("px-2", "px-4", undefined, { block: true })).toBe("px-4 block");
  });
});
