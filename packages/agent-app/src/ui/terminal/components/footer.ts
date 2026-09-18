import { isAbsolute, relative, resolve, sep } from "node:path";
import { type Component, truncateToWidth, visibleWidth } from "@liuxuedeng/agent-core-tui";
import type { AgentSession } from "../../../app/application.ts";
import { areExperimentalFeaturesEnabled } from "../../../app/experimental.ts";
import type { ReadonlyFooterDataProvider } from "../../../app/footer-data-provider.ts";
import { addUsageToTotals, createUsageTotals } from "../../../app/usage-totals.ts";
import { THINKING_LEVEL_LABELS } from "../../thinking-labels.ts";
import { theme } from "../theme/theme.ts";
import { keyDisplayText } from "./keybinding-hints.ts";

/**
 * Sanitize text for display in a single-line status.
 * Removes newlines, tabs, carriage returns, and other control characters.
 */
function sanitizeStatusText(text: string): string {
	// Replace newlines, tabs, carriage returns with space, then collapse multiple spaces
	return text
		.replace(/[\r\n\t]/g, " ")
		.replace(/ +/g, " ")
		.trim();
}

/**
 * Format token counts for compact footer display.
 */
export function formatTokens(count: number): string {
	if (count < 1000) return count.toString();
	if (count < 10000) return `${(count / 1000).toFixed(1)}k`;
	if (count < 1000000) return `${Math.round(count / 1000)}k`;
	if (count < 10000000) return `${(count / 1000000).toFixed(1)}M`;
	return `${Math.round(count / 1000000)}M`;
}

export function formatCwdForFooter(cwd: string, home: string | undefined): string {
	if (!home) return cwd;

	const resolvedCwd = resolve(cwd);
	const resolvedHome = resolve(home);
	const relativeToHome = relative(resolvedHome, resolvedCwd);
	const isInsideHome =
		relativeToHome === "" ||
		(relativeToHome !== ".." && !relativeToHome.startsWith(`..${sep}`) && !isAbsolute(relativeToHome));

	if (!isInsideHome) return cwd;
	return relativeToHome === "" ? "~" : `~${sep}${relativeToHome}`;
}

/**
 * Footer component that shows pwd, token stats, and context usage.
 * Computes token/context stats from session, gets git branch and extension statuses from provider.
 */
export class FooterComponent implements Component {
	private autoCompactEnabled = true;
	private session: AgentSession;
	private footerData: ReadonlyFooterDataProvider;

	constructor(session: AgentSession, footerData: ReadonlyFooterDataProvider) {
		this.session = session;
		this.footerData = footerData;
	}

	setSession(session: AgentSession): void {
		this.session = session;
	}

	setAutoCompactEnabled(enabled: boolean): void {
		this.autoCompactEnabled = enabled;
	}

	/**
	 * No-op: git branch caching now handled by provider.
	 * Kept for compatibility with existing call sites in interactive-mode.
	 */
	invalidate(): void {
		// No-op: git branch is cached/invalidated by provider
	}

	/**
	 * Clean up resources.
	 * Git watcher cleanup now handled by provider.
	 */
	dispose(): void {
		// Git watcher cleanup handled by provider
	}

	render(width: number): string[] {
		if (width <= 0) return [];
		const state = this.session.state;

		// Calculate cumulative usage from ALL session entries (not just post-compaction messages)
		const usageTotals = createUsageTotals();
		let latestCacheHitRate: number | undefined;

		for (const entry of this.session.sessionManager.getEntries()) {
			if (entry.type === "message" && entry.message.role === "assistant") {
				addUsageToTotals(usageTotals, entry.message.usage);

				const latestPromptTokens =
					entry.message.usage.input + entry.message.usage.cacheRead + entry.message.usage.cacheWrite;
				latestCacheHitRate =
					latestPromptTokens > 0 ? (entry.message.usage.cacheRead / latestPromptTokens) * 100 : undefined;
			} else if (entry.type === "message" && entry.message.role === "toolResult" && entry.message.usage) {
				addUsageToTotals(usageTotals, entry.message.usage);
			} else if ((entry.type === "branch_summary" || entry.type === "compaction") && entry.usage) {
				addUsageToTotals(usageTotals, entry.usage);
			}
		}

		// Calculate context usage from session (handles compaction correctly).
		// After compaction, tokens are unknown until the next LLM response.
		const contextUsage = this.session.getContextUsage();
		const contextWindow = contextUsage?.contextWindow ?? state.model?.contextWindow ?? 0;
		const contextPercentValue = contextUsage?.percent ?? 0;
		const contextPercent = contextUsage?.percent !== null ? contextPercentValue.toFixed(1) : "?";

		// Replace home directory with ~
		let pwd = formatCwdForFooter(this.session.sessionManager.getCwd(), process.env.HOME || process.env.USERPROFILE);

		// Add git branch if available
		const branch = this.footerData.getGitBranch();
		if (branch) {
			pwd = `${pwd} (${branch})`;
		}

		// Add session name if set
		const sessionName = this.session.sessionManager.getSessionName();
		if (sessionName) {
			pwd = `${pwd} • ${sessionName}`;
		}

		// Build stats line
		const statsParts: string[] = [];
		if (usageTotals.input) statsParts.push(`输入 ${formatTokens(usageTotals.input)}`);
		if (usageTotals.output) statsParts.push(`输出 ${formatTokens(usageTotals.output)}`);
		if ((usageTotals.cacheRead > 0 || usageTotals.cacheWrite > 0) && latestCacheHitRate !== undefined) {
			statsParts.push(`缓存命中 ${latestCacheHitRate.toFixed(1)}%`);
		}
		if (usageTotals.cacheRead) statsParts.push(`缓存读 ${formatTokens(usageTotals.cacheRead)}`);
		if (usageTotals.cacheWrite) statsParts.push(`缓存写 ${formatTokens(usageTotals.cacheWrite)}`);

		// Kimi Coding is subscription-backed despite using API-key authentication.
		const usingSubscription = state.model
			? state.model.provider === "kimi-coding" || this.session.modelRuntime.isUsingSubscription(state.model.provider)
			: false;
		if (usageTotals.cost || usingSubscription) {
			const costStr = `$${usageTotals.cost.toFixed(3)}${usingSubscription ? "（订阅）" : ""}`;
			statsParts.push(costStr);
		}

		// Colorize context percentage based on usage
		let contextPercentStr: string;
		const autoIndicator = this.autoCompactEnabled ? "（自动压缩）" : "";
		const contextPercentDisplay =
			contextPercent === "?"
				? `上下文 ?/${formatTokens(contextWindow)}${autoIndicator}`
				: `上下文 ${contextPercent}%/${formatTokens(contextWindow)}${autoIndicator}`;
		if (contextPercentValue > 90) {
			contextPercentStr = theme.fg("error", contextPercentDisplay);
		} else if (contextPercentValue > 70) {
			contextPercentStr = theme.fg("warning", contextPercentDisplay);
		} else {
			contextPercentStr = contextPercentDisplay;
		}
		statsParts.push(contextPercentStr);
		if (areExperimentalFeaturesEnabled()) {
			statsParts.push(`${theme.fg("dim", "•")} ${theme.bold(theme.fg("warning", "实验"))}`);
		}

		// Keep the built-in footer to two rows, even when the terminal is narrow.
		const lines: string[] = [];
		const addRow = (left: string, right: string): void => {
			const rightBudget =
				visibleWidth(left) + visibleWidth(right) + 2 <= width
					? visibleWidth(right)
					: Math.min(visibleWidth(right), Math.floor(width * 0.4));
			const rightText = truncateToWidth(right, rightBudget, "");
			const gap = rightText ? 2 : 0;
			const leftText = truncateToWidth(left, Math.max(0, width - visibleWidth(rightText) - gap), "…");
			lines.push(
				theme.fg("muted", leftText) +
					" ".repeat(Math.max(0, width - visibleWidth(leftText) - visibleWidth(rightText))) +
					theme.fg("muted", rightText),
			);
		};
		const hints = [
			["tui.input.submit", "发送"],
			["app.interrupt", "中止"],
			["app.clear", "清空"],
		] as const;
		const shortcuts = hints
			.flatMap(([action, label]) => {
				const keys = keyDisplayText(action);
				return keys ? [`${keys} ${label}`] : [];
			})
			.join(" · ");

		const model = state.model ? `${state.model.provider} / ${state.model.id}` : "未选择模型";
		const reasoning = state.model?.reasoning
			? `推理强度：${THINKING_LEVEL_LABELS[state.thinkingLevel || "off"]}`
			: "";
		addRow(shortcuts ? `${pwd}  ${shortcuts}` : pwd, model);
		addRow(statsParts.join(" · "), reasoning);

		// Add extension statuses on a single line, sorted by key alphabetically
		const extensionStatuses = this.footerData.getExtensionStatuses();
		if (extensionStatuses.size > 0) {
			const sortedStatuses = Array.from(extensionStatuses.entries())
				.sort(([a], [b]) => a.localeCompare(b))
				.map(([, text]) => sanitizeStatusText(text));
			const statusLine = sortedStatuses.join(" ");
			// Truncate to terminal width with dim ellipsis for consistency with footer style
			lines.push(truncateToWidth(statusLine, width, theme.fg("dim", "...")));
		}

		return lines;
	}
}
