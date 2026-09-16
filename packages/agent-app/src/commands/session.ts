/**
 * Application operations shared by the TUI, RPC, and SDK entry points.
 *
 * Each operation holds the business logic for one user action, exactly once.
 * UI adapters parse input, invoke the operation, and render the result; they
 * do not re-implement validation or session mutation themselves.
 */

import type { AgentSession } from "../app/application.ts";

export interface SessionNameOperationResult {
	/** The stored session name after the operation, when it succeeded. */
	applied?: string;
	/** The requested name, when normalization changed it before storing. */
	normalizedFrom?: string;
	/** Set when the input was rejected and the session was left unchanged. */
	rejected?: "empty";
}

/**
 * Set the display name of the current session.
 * Backs the TUI `/name` command and the RPC `set_session_name` request.
 */
export function setSessionName(session: AgentSession, rawName: string): SessionNameOperationResult {
	const name = rawName.trim();
	if (!name) {
		return { rejected: "empty" };
	}
	session.setSessionName(name);
	const applied = session.sessionManager.getSessionName();
	return applied === name ? { applied } : { applied, normalizedFrom: name };
}
