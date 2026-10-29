# Security

Please report a vulnerability privately, through
[GitHub's private vulnerability reporting](https://github.com/orascope/herdr-tower/security/advisories/new),
not in a public issue or pull request.

Include the version (`herdr-tower version`), the operating system, and the
steps that show the problem. The parts most worth a look are the remote MCP
server (`herdr-tower mcp`, reachable from outside the machine when you publish
it), the hooks the agents run, and the resident supervisor.

Fixes go into the latest release; older versions are not patched.
