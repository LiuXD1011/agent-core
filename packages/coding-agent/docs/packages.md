> Agent Core can help you create Agent Core packages. Ask it to bundle your extensions, skills, prompt templates, or themes.

# Agent Core Packages

Agent Core packages bundle extensions, skills, prompt templates, and themes so you can share them through npm or git. A package can declare resources in `package.json` under the `pi` key, or use conventional directories. Agent Core keeps the `pi` manifest key for resource declarations.

## Table of Contents

- [Install and Manage](#install-and-manage)
- [Package Sources](#package-sources)
- [Creating an Agent Core Package](#creating-an-agent-core-package)
- [Package Structure](#package-structure)
- [Dependencies](#dependencies)
- [Package Filtering](#package-filtering)
- [Enable and Disable Resources](#enable-and-disable-resources)
- [Scope and Deduplication](#scope-and-deduplication)

## Install and Manage

> **Security:** Agent Core packages run with full system access. Extensions execute arbitrary code, and skills can instruct the model to perform any action including running executables. Review source code before installing third-party packages.

```bash
agent-core install npm:@foo/bar@1.0.0
agent-core install git:github.com/user/repo@v1
agent-core install https://github.com/user/repo  # raw URLs work too
agent-core install /absolute/path/to/package
agent-core install ./relative/path/to/package

agent-core remove npm:@foo/bar
agent-core list                     # show installed packages from settings
agent-core update                   # update agent-core only
agent-core update --all             # update agent-core, update packages, and reconcile pinned git refs
agent-core update --extensions      # update packages and reconcile pinned git refs only
agent-core update --models          # refresh model catalogs only
agent-core update --self            # update agent-core only
agent-core update --self --force    # reinstall agent-core even if current
agent-core update npm:@foo/bar      # update one package
agent-core update --extension npm:@foo/bar
```

These commands manage Agent Core packages and `agent-core update` can update the agent-core CLI installation. For experimental installer-managed installations, `agent-core update` installs the exact checked version into a staged, lockfile-backed release and activates it only after verification, leaving the current release intact if the update fails. Managed installations do not support `--force`; rerun the installer to repair one. To uninstall agent-core itself, see [Uninstall](usage.md#uninstall).

By default, `install` and `remove` write to user settings (`~/.agent-core/agent/settings.json`). Use `-l` to write to project settings (`.agent-core/settings.json`) instead. Project settings can be shared with your team, and agent-core installs any missing packages automatically on startup after the project is trusted.

To try a package without installing it, use `--extension` or `-e`. This installs to a temporary directory for the current run only:

```bash
agent-core -e npm:@foo/bar
agent-core -e git:github.com/user/repo
```

## Package Sources

Agent Core accepts three source types in settings and `agent-core install`.

### npm

```
npm:@scope/pkg@1.2.3
npm:pkg
```

- Versioned specs are pinned and skipped by package updates (`agent-core update --extensions`, `agent-core update --all`).
- User installs go under `~/.agent-core/agent/npm/`.
- Project installs go under `.agent-core/npm/`.
- Set `npmCommand` in `settings.json` to pin npm package lookup and install operations to a specific wrapper command such as `mise` or `asdf`.

Example:

```json
{
  "npmCommand": ["mise", "exec", "node@20", "--", "npm"]
}
```

### git

```
git:github.com/user/repo@v1
git:git@github.com:user/repo@v1
https://github.com/user/repo@v1
ssh://git@github.com/user/repo@v1
```

- Without `git:` prefix, only protocol URLs are accepted (`https://`, `http://`, `ssh://`, `git://`).
- With `git:` prefix, shorthand formats are accepted, including `github.com/user/repo` and `git@github.com:user/repo`.
- HTTPS and SSH URLs are both supported.
- SSH URLs use your configured SSH keys automatically (respects `~/.ssh/config`).
- For non-interactive runs (for example CI), you can set `GIT_TERMINAL_PROMPT=0` to disable credential prompts and set `GIT_SSH_COMMAND` (for example `ssh -o BatchMode=yes -o ConnectTimeout=5`) to fail fast.
- Refs are pinned tags or commits. `agent-core update --extensions` and `agent-core update --all` do not move them to newer refs, but they do reconcile an existing clone to the configured ref.
- Use `agent-core install git:host/user/repo@new-ref` to update settings and move an existing package to a new pinned ref.
- Cloned to `~/.agent-core/agent/git/<host>/<path>` (global) or `.agent-core/git/<host>/<path>` (project).
- When reconciliation changes the checkout, agent-core resets and cleans the clone, then runs `npm install` if `package.json` exists.

**SSH examples:**
```bash
# git@host:path shorthand (requires git: prefix)
agent-core install git:git@github.com:user/repo

# ssh:// protocol format
agent-core install ssh://git@github.com/user/repo

# With version ref
agent-core install git:git@github.com:user/repo@v1.0.0
```

### Local Paths

```
/absolute/path/to/package
./relative/path/to/package
```

Local paths point to files or directories on disk and are added to settings without copying. Relative paths are resolved against the settings file they appear in. If the path is a file, it loads as a single extension. If it is a directory, agent-core loads resources using package rules.

## Creating an Agent Core Package

Add a `pi` manifest to `package.json` or use conventional directories. Include the `pi-package` keyword for discoverability.

```json
{
  "name": "my-package",
  "keywords": ["pi-package"],
  "pi": {
    "extensions": ["./extensions"],
    "skills": ["./skills"],
    "prompts": ["./prompts"],
    "themes": ["./themes"]
  }
}
```

Paths are relative to the package root. Arrays support glob patterns and `!exclusions`. Positive manifest globs discover visible paths in lexical order. List dot-prefixed paths directly. If a glob would need to continue through a symlink, list the symlinked resource root directly.

## Package Structure

### Convention Directories

If no `pi` manifest is present, Agent Core auto-discovers resources from these directories:

- `extensions/` loads `.ts` and `.js` files
- `skills/` recursively finds `SKILL.md` folders and loads top-level `.md` files as skills
- `prompts/` loads `.md` files
- `themes/` loads `.json` files

## Dependencies

Third party runtime dependencies belong in `dependencies` in `package.json`. Dependencies that do not register extensions, skills, prompt templates, or themes also belong in `dependencies`. When agent-core installs a package from npm or git, it runs `npm install`, so those dependencies are installed automatically.

Agent Core bundles core packages for extensions and skills. If you import any of these, list them in `peerDependencies` with a `"*"` range and do not bundle them: `@liuxuedeng/agent-core-ai`, `@liuxuedeng/agent-core-agent`, `@liuxuedeng/agent-core`, `@liuxuedeng/agent-core-tui`, `typebox`.

Other Agent Core packages must be bundled in your tarball. Add them to `dependencies` and `bundledDependencies`, then reference their resources through `node_modules/` paths. Agent Core loads packages with separate module roots, so separate installs do not collide or share modules.

Example:

```json
{
  "dependencies": {
    "shitty-extensions": "^1.0.1"
  },
  "bundledDependencies": ["shitty-extensions"],
  "pi": {
    "extensions": ["extensions", "node_modules/shitty-extensions/extensions"],
    "skills": ["skills", "node_modules/shitty-extensions/skills"]
  }
}
```

## Package Filtering

Filter what a package loads using the object form in settings:

```json
{
  "packages": [
    "npm:simple-pkg",
    {
      "source": "npm:my-package",
      "extensions": ["extensions/*.ts", "!extensions/legacy.ts"],
      "skills": [],
      "prompts": ["prompts/review.md"],
      "themes": ["+themes/legacy.json"]
    }
  ]
}
```

`+path` and `-path` are exact paths relative to the package root.

- Omit a key to load all of that type.
- Use `[]` to load none of that type.
- `!pattern` excludes matches.
- `+path` force-includes an exact path.
- `-path` force-excludes an exact path.
- Filters layer on top of the manifest. They narrow down what is already allowed.

## Enable and Disable Resources

Use `agent-core config` to enable or disable extensions, skills, prompt templates, and themes from installed packages and local directories. `agent-core config` starts in global settings (`~/.agent-core/agent/settings.json`); press Tab to switch between global and project-local modes. Use `agent-core config -l` to start in project overrides (`.agent-core/settings.json`) with inherited global resources dimmed.

## Scope and Deduplication

Packages can appear in both global and project settings. If the same package appears in both, the project entry wins unless the project entry has `autoload: false`, in which case it is applied as a delta over the global entry. Identity is determined by:

- npm: package name
- git: repository URL without ref
- local: resolved absolute path
