/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { join } from "node:path";
import type { ExtensionContext } from "../extensions/types.ts";

export function runtimeRoot(ctx: ExtensionContext): string {
	const sessionDir = ctx.sessionManager.getSessionDir();
	if (!sessionDir) throw new Error("Context requires a persistent session directory");
	const sessionId = ctx.sessionManager.getSessionId();
	if (!/^[a-z0-9][a-z0-9._-]*$/iu.test(sessionId)) {
		throw new Error("Context requires a valid session id");
	}
	return join(sessionDir, "context", sessionId);
}
