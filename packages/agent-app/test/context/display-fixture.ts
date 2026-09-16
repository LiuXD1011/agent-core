/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { type Component, Container, Text } from "@liuxuedeng/agent-core-tui";
import type { ExtensionContext } from "../../src/extensions/types.ts";
import type { Theme } from "../../src/ui/terminal/theme/theme.ts";

export type ContextTuiMechanism = "File update" | "Result reference" | "Log reduction" | "Context compaction";

const STATUS_KEY = "context-savings";
const STATUS_DURATION_MS = 4_000;
const INTEGER_FORMAT = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });
const statusTimers = new WeakMap<ExtensionContext["ui"], ReturnType<typeof setTimeout>>();

export function formatSavingsCount(value: number, unit: string): string {
	const count = Number.isFinite(value) ? Math.max(0, Math.round(value)) : 0;
	return `${INTEGER_FORMAT.format(count)} ${unit}`;
}

function compactDecimal(value: number): string {
	return value.toFixed(1).replace(/\.0$/u, "");
}

export function formatSavingsBytes(value: number): string {
	const bytes = Number.isFinite(value) ? Math.max(0, Math.round(value)) : 0;
	if (bytes >= 1024 * 1024) {
		return `${compactDecimal(bytes / (1024 * 1024))} MiB removed from future prompts`;
	}
	if (bytes >= 1024) return `${compactDecimal(bytes / 1024)} KiB removed from future prompts`;
	return `${INTEGER_FORMAT.format(bytes)} B removed from future prompts`;
}

export function renderContextTool(
	theme: Theme,
	mechanism: ContextTuiMechanism,
	saving: string,
	base?: Component,
): Component {
	const container = new Container();
	const title = theme.fg("accent", theme.bold(mechanism));
	container.addChild(new Text(title, 0, 0));
	container.addChild(new Text(theme.fg("success", `Context · ${saving}`), 0, 0));
	if (base) container.addChild(base);
	return container;
}

export function showContextSavings(context: ExtensionContext, mechanism: ContextTuiMechanism, saving: string): void {
	if (context.mode !== "tui") return;
	const message = `${mechanism}\nContext · ${saving}`;
	context.ui.notify(message, "info");
	context.ui.setStatus(STATUS_KEY, `${mechanism} · ${saving}`);

	const previous = statusTimers.get(context.ui);
	if (previous) clearTimeout(previous);
	const timer = setTimeout(() => {
		if (statusTimers.get(context.ui) !== timer) return;
		statusTimers.delete(context.ui);
		context.ui.setStatus(STATUS_KEY, undefined);
	}, STATUS_DURATION_MS);
	if (typeof timer === "object" && "unref" in timer) timer.unref();
	statusTimers.set(context.ui, timer);
}
