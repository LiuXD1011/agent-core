import { APP_NAME } from "../config.ts";
import { configureHttpDispatcher } from "../core/http-dispatcher.ts";
import { applyProcessMarkers } from "./process-markers.ts";

export function setupCli(): void {
	applyProcessMarkers(APP_NAME);

	// Configure undici before provider SDKs issue requests. Settings are applied
	// once SettingsManager has loaded global/project configuration.
	configureHttpDispatcher();
}
