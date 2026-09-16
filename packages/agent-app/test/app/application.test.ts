import { existsSync, mkdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { getModel } from "@liuxuedeng/agent-core-ai/compat";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { setSessionName } from "../../src/commands/session.ts";
import { createAgentSession } from "../../src/sdk.ts";

describe("application session operations", () => {
	let tempDir: string;

	beforeEach(() => {
		tempDir = join(tmpdir(), `pi-app-op-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(join(tempDir, "project"), { recursive: true });
		mkdirSync(join(tempDir, "agent"), { recursive: true });
	});

	afterEach(() => {
		if (tempDir && existsSync(tempDir)) {
			rmSync(tempDir, { recursive: true, force: true });
		}
	});

	it("setSessionName stores a trimmed name and reports normalization", async () => {
		const model = getModel("anthropic", "claude-sonnet-4-5");
		expect(model).toBeTruthy();
		const { session } = await createAgentSession({
			cwd: join(tempDir, "project"),
			agentDir: join(tempDir, "agent"),
			model: model!,
		});

		const applied = setSessionName(session, "  my session  ");
		expect(applied).toEqual({ applied: "my session" });
		expect(session.sessionManager.getSessionName()).toBe("my session");

		// appendSessionInfo collapses newlines; report the normalized input
		const normalized = setSessionName(session, "bad\nname");
		expect(normalized.applied).toBe("bad name");
		expect(normalized.normalizedFrom).toBe("bad\nname");

		// Empty input is rejected without touching the session
		const rejected = setSessionName(session, "   ");
		expect(rejected).toEqual({ rejected: "empty" });
		expect(session.sessionManager.getSessionName()).toBe("bad name");

		session.dispose();
	});
});
