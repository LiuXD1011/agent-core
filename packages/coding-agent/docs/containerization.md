# Containerization

Agent Core runs with all permissions by default, but in some cases, you will want to have more control over what directories Agent Core can write to and which accesses it has.

There are two general options. You can either
1. run the whole `agent-core` process inside an isolated environment, or
2. run `agent-core` on the host and route tool execution into an isolated environment.

## Choose a pattern

| Pattern | What is isolated | Best for | Notes |
| --- | --- | --- | --- |
| Gondolin extension | Built-in tools and `!` commands | Local micro-VM isolation while keeping auth on host | See [`examples/extensions/gondolin/`](../examples/extensions/gondolin/). |
| Plain Docker | Whole `agent-core` process in a local container | Simple local isolation | Provider API keys enter the container. |

Extensions run wherever the `agent-core` process runs. If you run host `agent-core` with a tool-routing extension, other custom extension tools still run on the host unless they also delegate their operations.

Managed sandbox services (for example NVIDIA OpenShell or Docker Sandboxes) are intentionally not documented here; only the integrations below are verified against this repository.

## Gondolin

[Gondolin](https://github.com/earendil-works/gondolin) is a local Linux micro-VM.
Use the [example extension](../examples/extensions/gondolin) when you want `agent-core` on the host but all built-in tools routed into the VM.

Setup:

```bash
cp -R packages/coding-agent/examples/extensions/gondolin ~/.agent-core/agent/extensions/gondolin
cd ~/.agent-core/agent/extensions/gondolin
npm install --ignore-scripts
```

Run from the project you want mounted:

```bash
cd /path/to/project
agent-core -e ~/.agent-core/agent/extensions/gondolin
```

The extension mounts the host cwd at `/workspace` in the VM and overrides `read`, `write`, `edit`, `bash`, `grep`, `find`, and `ls`.
User `!` commands are routed into the VM, as well.
File changes under `/workspace` write through to the host.

Requirements: Node.js >= 23.6.0 for `@earendil-works/gondolin`, plus QEMU (requires installation through your package manager).

## Plain Docker

Run the whole `agent-core` process in Docker when you want the simplest local container boundary.

`Dockerfile.agent-core`:

```dockerfile
FROM node:24-bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends bash ca-certificates git ripgrep \
  && rm -rf /var/lib/apt/lists/*
RUN npm install -g --ignore-scripts @liuxuedeng/agent-core

WORKDIR /workspace
ENTRYPOINT ["agent-core"]
```

Build and run:

```bash
docker build -t agent-core-sandbox -f Dockerfile.agent-core .

docker run --rm -it \
  -e ANTHROPIC_API_KEY \
  -v "$PWD:/workspace" \
  -v agent-core-agent-home:/root/.agent-core/agent \
  agent-core-sandbox
```

The `-v "$PWD:/workspace"` mounts your current directory into the container at /workspace such that reads and writes in `/workspace` inside Docker directly affect your host files, like in the Gondolin example.

Use a named volume for `/root/.agent-core/agent` if you want container-local settings and sessions. Mounting your host `~/.agent-core/agent` exposes host auth and session files to the container.
