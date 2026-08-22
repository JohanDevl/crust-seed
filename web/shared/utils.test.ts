import { describe, expect, it } from "vitest";

import { formatAsList, humanReadableSize } from "./utils.js";

describe("formatAsList", () => {
	it("sorts values before formatting when requested", () => {
		expect(formatAsList(["beta", "alpha"], { sort: true })).toBe(
			"alpha and beta",
		);
	});
});

describe("humanReadableSize", () => {
	it("formats decimal byte sizes", () => {
		expect(humanReadableSize(1_500)).toBe("1.5 kB");
	});

	it("formats binary byte sizes", () => {
		expect(humanReadableSize(1_536, { binary: true })).toBe("1.5 KiB");
	});
});
