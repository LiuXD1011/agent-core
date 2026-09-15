/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { type Component, Container, Text } from "@liuxuedeng/agent-core-tui";
import type { ExtensionContext, Theme } from "./host.ts";

export type EfficiencyTuiMechanism =
	| "Action Fusion"
	| "Observation Pack"
	| "Evidence Reducer"
	| "Online Context Compact";

const STATUS_KEY = "efficiency-savings";
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

export function renderEfficiencyTool(
	theme: Theme,
	mechanism: EfficiencyTuiMechanism,
	saving: string,
	base?: Component,
): Component {
	const container = new Container();
	const title = theme.fg("accent", theme.bold(mechanism));
	container.addChild(new Text(title, 0, 0));
	container.addChild(new Text(theme.fg("success", `Efficiency · ${saving}`), 0, 0));
	if (base) container.addChild(base);
	return container;
}

export function showEfficiencySavings(
	context: ExtensionContext,
	mechanism: EfficiencyTuiMechanism,
	saving: string,
): void {
	if (context.mode !== "tui") return;
	const message = `${mechanism}\nEfficiency · ${saving}`;
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
