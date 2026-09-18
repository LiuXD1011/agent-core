import type { ThinkingLevel } from "@liuxuedeng/agent-core-agent";

/** Display labels only; model requests and stored settings keep ThinkingLevel values. */
export const THINKING_LEVEL_LABELS: Record<ThinkingLevel, string> = {
	off: "关闭",
	minimal: "极低",
	low: "低",
	medium: "中",
	high: "高",
	xhigh: "超高",
	max: "最高",
};

export const THINKING_LEVEL_DESCRIPTIONS: Record<ThinkingLevel, string> = {
	off: "关闭可配置的推理模式（是否支持由模型决定）",
	minimal: "尽量减少推理投入",
	low: "较低的推理投入",
	medium: "适中的推理投入",
	high: "较高的推理投入",
	xhigh: "更高的推理投入",
	max: "模型支持的最高推理投入",
};
