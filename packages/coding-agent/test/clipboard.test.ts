import { execFileSync } from "child_process";
import { platform } from "os";
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { readClipboardText } from "../src/utils/clipboard.ts";

const mocks = vi.hoisted(() => {
	return {
		clipboard: {
			getText: vi.fn<() => Promise<string>>(),
		},
		execFileSync: vi.fn(),
		platform: vi.fn<() => NodeJS.Platform>(),
		isWaylandSession: vi.fn<() => boolean>(),
	};
});

vi.mock("../src/utils/clipboard-native.js", () => {
	return {
		clipboard: mocks.clipboard,
	};
});

vi.mock("child_process", () => {
	return {
		execFileSync: mocks.execFileSync,
	};
});

vi.mock("os", () => {
	return {
		platform: mocks.platform,
	};
});

vi.mock("../src/utils/clipboard-image.js", () => {
	return {
		isWaylandSession: mocks.isWaylandSession,
	};
});

const mockedExecFileSync = vi.mocked(execFileSync);
const mockedPlatform = vi.mocked(platform);

beforeEach(() => {
	vi.unstubAllEnvs();
	mocks.clipboard.getText.mockReset();
	mocks.execFileSync.mockReset();
	mocks.platform.mockReset();
	mocks.isWaylandSession.mockReset();
	mockedPlatform.mockReturnValue("darwin");
	mocks.isWaylandSession.mockReturnValue(false);
	mocks.clipboard.getText.mockResolvedValue("");
});

afterEach(() => {
	vi.unstubAllEnvs();
});

describe("readClipboardText", () => {
	test("returns native clipboard text", async () => {
		mocks.clipboard.getText.mockResolvedValue("clipboard text");

		await expect(readClipboardText()).resolves.toBe("clipboard text");
	});

	test("reads the Wayland clipboard before the stale native X11 clipboard", async () => {
		// Regression test for #7248.
		mockedPlatform.mockReturnValue("linux");
		mocks.isWaylandSession.mockReturnValue(true);
		vi.stubEnv("WAYLAND_DISPLAY", "wayland-0");
		mockedExecFileSync.mockReturnValue("Wayland text");
		mocks.clipboard.getText.mockResolvedValue("stale X11 text");

		await expect(readClipboardText()).resolves.toBe("Wayland text");
		expect(mockedExecFileSync).toHaveBeenCalledWith("wl-paste", ["--no-newline", "--type", "text"], {
			encoding: "utf8",
			maxBuffer: 50 * 1024 * 1024,
			timeout: 5000,
		});
		expect(mocks.clipboard.getText).not.toHaveBeenCalled();
	});

	test("does not fall back to stale X11 text when the Wayland clipboard is empty", async () => {
		mockedPlatform.mockReturnValue("linux");
		mocks.isWaylandSession.mockReturnValue(true);
		vi.stubEnv("WAYLAND_DISPLAY", "wayland-0");
		mockedExecFileSync.mockReturnValue("");
		mocks.clipboard.getText.mockResolvedValue("stale X11 text");

		await expect(readClipboardText()).resolves.toBeNull();
		expect(mocks.clipboard.getText).not.toHaveBeenCalled();
	});

	test("falls back to the native clipboard when wl-paste is unavailable", async () => {
		mockedPlatform.mockReturnValue("linux");
		mocks.isWaylandSession.mockReturnValue(true);
		vi.stubEnv("WAYLAND_DISPLAY", "wayland-0");
		mockedExecFileSync.mockImplementation(() => {
			throw new Error("wl-paste unavailable");
		});
		mocks.clipboard.getText.mockResolvedValue("X11 fallback text");

		await expect(readClipboardText()).resolves.toBe("X11 fallback text");
	});

	test("returns null for empty or unavailable clipboard text", async () => {
		await expect(readClipboardText()).resolves.toBeNull();

		mocks.clipboard.getText.mockRejectedValue(new Error("clipboard unavailable"));
		await expect(readClipboardText()).resolves.toBeNull();
	});
});
