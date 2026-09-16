# Security

## Execution boundaries

- Agent Core runs with the permissions of the account that starts it. It can read and modify files, execute commands, and access network services and credentials available to that process.
- Agent Core does not provide a built-in permission sandbox. Review its actions; use a container or virtual machine when isolation is needed. See [containerization guidance](packages/agent-app/docs/containerization.md).
- Trust the repositories, extensions, and skills you load. Repository instructions, comments, and tool output can influence model behavior; extensions can execute code. Prompt instructions are not a substitute for operating-system isolation.
- An attacker who can already change your workspace, configuration, or shell environment may influence the Agent. Reports involving prior local write access should explain any additional access or boundary crossing introduced by Agent Core.

## Reporting a vulnerability

Report privately through this repository's GitHub Security Advisories or contact the maintainer. Include the affected version or commit, impact, reproduction steps, and relevant logs with credentials removed.
