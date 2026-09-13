import { existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { PACKAGE_NAME, VERSION } from "../src/config.ts";
import { runManagedSelfUpdate } from "../src/package-manager-cli.ts";

describe("managed self-update boundaries", () => {
	const roots: string[] = [];
	const savedEnv = {
		installerApiBase: process.env.AGENT_CORE_INSTALLER_API_BASE,
		managedRoot: process.env.AGENT_CORE_MANAGED_INSTALL_ROOT,
	};
	let httpServer: Server | undefined;
	let serverUrl = "";

	function makeManagedRoot(): string {
		const root = mkdtempSync(join(tmpdir(), "agent-core-managed-install-"));
		roots.push(root);
		mkdirSync(join(root, "update"), { recursive: true });
		process.env.AGENT_CORE_MANAGED_INSTALL_ROOT = root;
		return root;
	}

	beforeEach(() => {
		delete process.env.AGENT_CORE_INSTALLER_API_BASE;
	});

	afterEach(async () => {
		await new Promise<void>((resolve) => {
			if (!httpServer) return resolve();
			httpServer.close(() => resolve());
		});
		httpServer = undefined;
		for (const root of roots.splice(0)) {
			rmSync(root, { recursive: true, force: true });
		}
		if (savedEnv.installerApiBase === undefined) delete process.env.AGENT_CORE_INSTALLER_API_BASE;
		else process.env.AGENT_CORE_INSTALLER_API_BASE = savedEnv.installerApiBase;
		if (savedEnv.managedRoot === undefined) delete process.env.AGENT_CORE_MANAGED_INSTALL_ROOT;
		else process.env.AGENT_CORE_MANAGED_INSTALL_ROOT = savedEnv.managedRoot;
	});

	async function serveManifests(respond: (version: string) => { status: number; body: string }): Promise<void> {
		httpServer = createServer((request, response) => {
			const parts = (request.url ?? "").split("/").filter(Boolean);
			const version = decodeURIComponent(parts[parts.length - 2] ?? "");
			const { status, body } = respond(version);
			response.statusCode = status;
			response.setHeader("content-type", "application/json");
			response.end(body);
		});
		const address = await new Promise<{ port: number }>((resolve, reject) => {
			httpServer?.once("listening", () => resolve(httpServer?.address() as { port: number }));
			httpServer?.once("error", reject);
			httpServer?.listen(0, "127.0.0.1");
		});
		serverUrl = `http://127.0.0.1:${address.port}/releases`;
	}

	it("terminates with an npm hint before writing anything when no install source is configured", async () => {
		const root = makeManagedRoot();

		await expect(runManagedSelfUpdate(root, "9.9.9")).rejects.toThrow(
			/npm install -g @liuxuedeng\/agent-core@latest/,
		);

		// No staging, no releases, no downloads: the managed root stays untouched.
		expect(existsSync(join(root, "staging"))).toBe(false);
		expect(existsSync(join(root, "releases"))).toBe(false);
		expect(existsSync(join(root, "current-version"))).toBe(false);
	});

	it("refuses to activate a manifest that serves a different product", async () => {
		const root = makeManagedRoot();
		await serveManifests(() => ({
			status: 200,
			body: JSON.stringify({ name: "@earendil-works/pi-coding-agent", version: "9.9.9" }),
		}));
		process.env.AGENT_CORE_INSTALLER_API_BASE = serverUrl;

		await expect(runManagedSelfUpdate(root, "9.9.9")).rejects.toThrow(/Refusing to install a different product/);

		expect(existsSync(join(root, "releases", "9.9.9"))).toBe(false);
		expect(existsSync(join(root, "current-version"))).toBe(false);
	});

	it("downloads only from the configured Agent Core release feed", async () => {
		const root = makeManagedRoot();
		const requested: string[] = [];
		await serveManifests((version) => {
			requested.push(version);
			return { status: 200, body: JSON.stringify({ name: PACKAGE_NAME, version }) };
		});
		process.env.AGENT_CORE_INSTALLER_API_BASE = serverUrl;

		// npm ci runs against the fake manifest and the missing agent-core binary
		// fails verification; the point of this test is that the fetches target
		// the configured feed with this product's name, never a default
		// upstream endpoint. One request per artifact (package.json + lockfile).
		await expect(runManagedSelfUpdate(root, VERSION)).rejects.toThrow();
		expect(requested).toEqual([VERSION, VERSION]);
	});
});
