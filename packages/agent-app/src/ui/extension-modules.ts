/** Bundled extension presentation APIs are assembled at the outer entry point. */
import * as tui from "@liuxuedeng/agent-core-tui";
import { registerExtensionModules } from "../extensions/loader.ts";
import * as applicationApi from "../index.ts";

registerExtensionModules({
	"@liuxuedeng/agent-core": applicationApi,
	"@liuxuedeng/agent-core-tui": tui,
	"@mariozechner/pi-coding-agent": applicationApi,
	"@mariozechner/pi-tui": tui,
});
