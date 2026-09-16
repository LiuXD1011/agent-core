#!/usr/bin/env node
import "./ui/extension-modules.ts";
import { setupCli } from "./cli/setup.ts";
import { main } from "./main.ts";

setupCli();
main(process.argv.slice(2));
