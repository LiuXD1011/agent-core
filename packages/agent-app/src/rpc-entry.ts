#!/usr/bin/env node
import "./ui/extension-modules.ts";
import { configureHttpDispatcher } from "./app/http-dispatcher.ts";
import { applyProcessMarkers } from "./cli/process-markers.ts";
import { APP_NAME } from "./config.ts";
import { main } from "./main.ts";

applyProcessMarkers(`${APP_NAME}-rpc`);

configureHttpDispatcher();

main(["--mode", "rpc", ...process.argv.slice(2)]);
