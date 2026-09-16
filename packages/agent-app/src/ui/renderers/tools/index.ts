/**
 * Built-in tool renderers, without the tools themselves.
 *
 * A presentation displays tool calls and results; it does not execute them and does not need their
 * typebox parameter schemas. Importing this instead of `core/tools/index.ts` keeps ~17 MB of module
 * graph out of a process that only renders.
 */

import { Container, Text } from "@liuxuedeng/agent-core-tui";
import type { ToolDefinition } from "../../../extensions/types.ts";
import type { ToolName } from "../../../tools/index.ts";
import { createShellRenderers } from "./bash.ts";
import { editRenderers } from "./edit.ts";
import { findRenderers } from "./find.ts";
import { grepRenderers } from "./grep.ts";
import { lsRenderers } from "./ls.ts";
import { readRenderers } from "./read.ts";
import { writeRenderers } from "./write.ts";

export type ToolRenderers = Pick<ToolDefinition<any, any>, "renderCall" | "renderResult">;

export {
	createShellRenderers,
	editRenderers,
	findRenderers,
	grepRenderers,
	lsRenderers,
	readRenderers,
	writeRenderers,
};

/** Renderers for every built-in tool, keyed by tool name. */
export function createAllToolRenderers(): Record<ToolName, ToolRenderers> {
	return {
		read: readRenderers,
		bash: createShellRenderers("$"),
		powershell: createShellRenderers("PS>"),
		edit: editRenderers,
		write: writeRenderers,
		grep: grepRenderers,
		find: findRenderers,
		ls: lsRenderers,
	};
}

/**
 * Merge built-in renderers into a tool definition that does not supply its own.
 *
 * `ToolExecutionComponent` used to do this lookup itself, which forced every presentation to import
 * the tool implementations. Callers do it now, so a process that renders can import renderers alone.
 */
export function withBuiltInRenderers<TDefinition extends ToolRenderers>(
	toolName: string,
	definition: TDefinition | undefined,
): TDefinition | ToolRenderers | undefined {
	let builtIn = createAllToolRenderers()[toolName as ToolName];
	if (builtIn && (toolName === "write" || toolName === "edit")) {
		const base = builtIn;
		builtIn = {
			renderCall(args, theme, context) {
				const command = (args as { then_run?: { command?: string } }).then_run?.command;
				if (!command) return base.renderCall!(args, theme, context);
				const component = new Container();
				component.addChild(base.renderCall!(args, theme, { ...context, lastComponent: undefined }));
				component.addChild(new Text(theme.fg("toolOutput", `\n$ ${command}`), 0, 0));
				return component;
			},
			renderResult(result, options, theme, context) {
				if (!(context.args as { then_run?: unknown } | undefined)?.then_run) {
					return base.renderResult!(result, options, theme, context);
				}
				const output = result.content.flatMap((block) => (block.type === "text" ? [block.text] : [])).join("\n");
				return new Text(theme.fg(context.isError ? "error" : "toolOutput", output), 0, 0);
			},
		};
	}
	if (!definition) return builtIn;
	if (!builtIn) return definition;
	return {
		...definition,
		renderCall: definition.renderCall ?? builtIn.renderCall,
		renderResult: definition.renderResult ?? builtIn.renderResult,
	};
}
