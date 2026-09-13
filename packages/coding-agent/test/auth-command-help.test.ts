import { afterEach, describe, expect, it, vi } from "vitest";
import { isAuthCommandHelp, printAuthCommandHelp } from "../src/cli/auth-command.ts";
import { APP_NAME } from "../src/config.ts";

describe("auth command help", () => {
	afterEach(() => {
		vi.restoreAllMocks();
	});

	it("uses APP_NAME and never hardcodes the old pi command", () => {
		const log = vi.spyOn(console, "log").mockImplementation(() => {});
		printAuthCommandHelp();
		const output = log.mock.calls.map((args) => args.join(" ")).join("\n");
		expect(output).toContain(`${APP_NAME} auth print-api-key`);
		expect(output).toContain(`${APP_NAME} auth print-bearer-token`);
		expect(output).toContain(`${APP_NAME} auth check`);
		expect(output).not.toMatch(/\bpi auth\b/);
	});

	it("recognizes bare auth, auth help, -h, and --help as help requests", () => {
		expect(isAuthCommandHelp(["auth"])).toBe(true);
		expect(isAuthCommandHelp(["auth", "help"])).toBe(true);
		expect(isAuthCommandHelp(["auth", "-h"])).toBe(true);
		expect(isAuthCommandHelp(["auth", "--help"])).toBe(true);
		expect(isAuthCommandHelp(["auth", "check"])).toBe(false);
		expect(isAuthCommandHelp(["login"])).toBe(false);
	});
});
