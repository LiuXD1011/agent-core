# Security Policy

This document explains the security model of Agent Core and where its boundaries are.

Agent Core is a coding agent that runs locally within the security boundary of the
user running it. It is the user's responsibility to monitor its operations or to
contain it within a container, virtual machine, or other sandbox solution. See
[packages/coding-agent/docs/containerization.md](packages/coding-agent/docs/containerization.md)
for isolation patterns.

Agent Core treats the local user account and files writable by that account as
inside the same trust boundary as the agent process itself. If an attacker can
modify files under the user's home directory, workspace, shell startup files,
environment, or agent configuration, they can generally influence the agent or
other local developer tools. Reports that depend on such prior local write
access are not security vulnerabilities unless they demonstrate how Agent Core
grants that write access or crosses an operating-system privilege boundary.

Agent Core relies on users installing trustworthy extensions, loading trustworthy
skills, and only running the agent within trusted repositories. Files like
`AGENTS.md` or instructions in comments can trivially prompt-inject a coding
agent; this cannot be protected against in general.

## Reporting a Vulnerability

If you believe you found a security vulnerability in Agent Core, open a private
report through GitHub Security Advisories for this repository, or contact the
maintainer directly. Vulnerabilities in upstream Pi itself should be reported
to the upstream project at <https://github.com/earendil-works/pi>.

Please include:

- A description of the issue and its impact
- Steps to reproduce, proof of concept, or relevant logs
- Affected package, version, commit, or configuration
- Any known mitigations
