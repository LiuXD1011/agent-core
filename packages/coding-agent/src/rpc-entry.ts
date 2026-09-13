#!/usr/bin/env node
import { applyProcessMarkers } from "./cli/process-markers.ts";
import { APP_NAME } from "./config.ts";
import { configureHttpDispatcher } from "./core/http-dispatcher.ts";
import { main } from "./main.ts";

applyProcessMarkers(`${APP_NAME}-rpc`);

configureHttpDispatcher();

main(["--mode", "rpc", ...process.argv.slice(2)]);
