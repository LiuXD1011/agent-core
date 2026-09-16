import type { ExtensionAPI, ExtensionFactory } from "../../src/extensions/types.ts";
import { type ActionFusionOptions, createFusedMutationToolDefinitions } from "../../src/tools/mutation/index.ts";

export function createActionFusionExtension(options: ActionFusionOptions = {}): ExtensionFactory {
	const definitions = createFusedMutationToolDefinitions(options);
	return (pi: ExtensionAPI) => {
		if (!options.tools || options.tools.includes("edit")) pi.registerTool(definitions.edit);
		if (!options.tools || options.tools.includes("write")) pi.registerTool(definitions.write);
	};
}
