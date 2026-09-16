import { APP_NAME } from "../config.ts";

/**
 * Mark the process as a Agent Core coding-agent entry point (CLI or RPC).
 * Generic tooling reads AI_AGENT; child processes read AGENT_CORE_CODING_AGENT.
 */
export function applyProcessMarkers(processTitle: string): void {
	process.title = processTitle;
	process.env.AGENT_CORE_CODING_AGENT = "true";
	process.env.AI_AGENT = APP_NAME;
	process.emitWarning = (() => {}) as typeof process.emitWarning;
}
