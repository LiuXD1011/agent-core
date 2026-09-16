/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { runtimeRoot } from "../../src/session/artifact-paths.ts";
import type { ExtensionContext } from "./host.ts";

function context(sessionDir: string, sessionId: string): ExtensionContext {
	return {
		sessionManager: {
			getSessionDir: () => sessionDir,
			getSessionId: () => sessionId,
		},
	} as unknown as ExtensionContext;
}

describe("Context runtime root", () => {
	it("gives each Pi session its own directory", () => {
		const sessionDir = join("sessions", "project-a");
		expect(runtimeRoot(context(sessionDir, "session-a"))).toBe(join(sessionDir, "context", "session-a"));
		expect(runtimeRoot(context(sessionDir, "session-b"))).toBe(join(sessionDir, "context", "session-b"));
	});

	it.each(["", ".", "..", "../escape", "nested/session", "nested\\session"])(
		"rejects unsafe session id %j",
		(sessionId) => {
			expect(() => runtimeRoot(context("sessions", sessionId))).toThrow("valid session id");
		},
	);
});
