// Explicit host bindings prevent a dependency cycle through the public package entrypoint.
export { CONFIG_DIR_NAME, getAgentDir } from "../../config.ts";
export type { Theme } from "../../modes/interactive/theme/theme.ts";
export { estimateTokens, findCutPoint } from "../compaction/compaction.ts";
export type {
	CompactOptions,
	ExtensionAPI,
	ExtensionContext,
	ExtensionFactory,
	ToolDefinition,
	ToolResultEvent,
} from "../extensions/types.ts";
export type { SessionEntry } from "../session-manager.ts";
export { buildSessionContext, sessionEntryToContextMessages } from "../session-manager.ts";
export type { BashOperations, BashToolOptions } from "../tools/bash.ts";
export { createBashToolDefinition } from "../tools/bash.ts";
export type { EditToolDetails, EditToolOptions } from "../tools/edit.ts";
export { createEditToolDefinition } from "../tools/edit.ts";
export type { WriteToolOptions } from "../tools/write.ts";
export { createWriteToolDefinition } from "../tools/write.ts";
