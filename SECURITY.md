# Security policy

## Supported versions

Only the latest release gets security fixes. Update with **Update now** in the app, or download it from [Releases](https://github.com/lClayk/Rojo-Manager/releases/latest).

## Reporting a vulnerability

Report it privately through [GitHub's vulnerability reporting](https://github.com/lClayk/Rojo-Manager/security/advisories/new) (**Security** tab, **Report a vulnerability**). Please do not open a public issue or pull request for it.

Include the Rojo Manager version, what an attacker can do, and the steps to reproduce it. You should get a reply within a week. Once a fix is released, the advisory is published with credit to you, unless you would rather stay anonymous.

## Scope

These parts handle untrusted input and matter most:

- The local agent API, which needs the token in `%LOCALAPPDATA%\RojoManager\control.json` and refuses requests from browsers.
- The auto-connect listener on `localhost:34871`, which answers without a token.
- The Rojo bridge plugin (`rojo_bridge`), which any Studio plugin can call.
- The app updater and the tool installer, which download and run files from GitHub releases.
- `RojoManagerMCP.exe` and the arguments agents pass to its tools.

Out of scope: problems in Rojo, Rokit, Wally, Lune, robloxstudio-mcp, or Roblox Studio themselves (report those to their projects), and attacks that already need full control of your Windows account.
