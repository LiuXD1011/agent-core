import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const dependencySections = ["dependencies", "devDependencies", "optionalDependencies"];
const exactVersionPattern = /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const ignoredDirectories = new Set([".git", "dist", "node_modules"]);
const packageJsonFiles = [];

function collectPackageJsonFiles(directory) {
	for (const entry of readdirSync(directory, { withFileTypes: true })) {
		if (entry.isDirectory()) {
			if (!ignoredDirectories.has(entry.name)) {
				collectPackageJsonFiles(join(directory, entry.name));
			}
			continue;
		}

		if (entry.isFile() && entry.name === "package.json") {
			packageJsonFiles.push(join(directory, entry.name));
		}
	}
}

function collectWorkspacePackageNames() {
	const rootPackageJson = JSON.parse(readFileSync("package.json", "utf8"));
	const names = new Set();
	const manifestPaths = [];
	for (const pattern of rootPackageJson.workspaces ?? []) {
		const starIndex = pattern.indexOf("*");
		if (starIndex === -1) {
			manifestPaths.push(pattern);
			continue;
		}
		const parent = pattern.slice(0, starIndex);
		const suffix = pattern.slice(starIndex + 1);
		for (const entry of readdirSync(parent, { withFileTypes: true })) {
			if (entry.isDirectory()) {
				manifestPaths.push(`${parent}${entry.name}${suffix}`);
			}
		}
	}
	for (const manifestPath of manifestPaths) {
		const manifestFile = join(manifestPath, "package.json");
		if (existsSync(manifestFile)) {
			const name = JSON.parse(readFileSync(manifestFile, "utf8")).name;
			if (typeof name === "string") {
				names.add(name);
			}
		}
	}
	return names;
}

const workspacePackageNames = collectWorkspacePackageNames();

function isInternalWorkspaceDependency(name) {
	return workspacePackageNames.has(name);
}

function isNonRegistrySpecifier(specifier) {
	return /^(?:workspace:|file:|link:|portal:|git\+|github:|git:|https?:|ssh:|git:\/\/)/.test(specifier);
}

function getVersionSpecifier(specifier) {
	if (!specifier.startsWith("npm:")) return specifier;
	const aliasTarget = specifier.slice("npm:".length);
	const versionSeparator = aliasTarget.lastIndexOf("@");
	if (versionSeparator <= 0) return specifier;
	return aliasTarget.slice(versionSeparator + 1);
}

const failures = [];

collectPackageJsonFiles(".");

for (const file of packageJsonFiles.sort()) {
	const packageJson = JSON.parse(readFileSync(file, "utf8"));

	for (const section of dependencySections) {
		const dependencies = packageJson[section];
		if (!dependencies) continue;

		for (const [name, specifier] of Object.entries(dependencies)) {
			if (isInternalWorkspaceDependency(name) || isNonRegistrySpecifier(specifier)) continue;
			if (exactVersionPattern.test(getVersionSpecifier(specifier))) continue;
			failures.push(`${file}: ${section}.${name} must be pinned, found ${specifier}`);
		}
	}
}

if (failures.length > 0) {
	console.error("Direct external dependencies must use exact versions:");
	for (const failure of failures) console.error(`  ${failure}`);
	process.exit(1);
}
