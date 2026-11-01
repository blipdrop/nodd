# Nodd CLI

Send notifications to your iPhone and wait for a human response from scripts, CI, and AI agents.

Nodd is an iOS app. This repository contains its command-line client and the public API specification. The service itself is not open source.

## Install

```sh
brew install blipdrop/tap/nodd
```

Or download a binary from [Releases](https://github.com/blipdrop/nodd/releases).

## Get started

Run `nodd login` and scan the QR code with the Nodd app. The send key is stored in `~/.config/nodd/config.json`.

```sh
nodd send "Deploy finished" --body "Version 1.4.2 is live"

nodd ask "Deploy to production?"
```

`nodd ask` exit codes:

| Code | Meaning |
| --- | --- |
| `0` | Approved or an option was chosen |
| `10` | Rejected |
| `11` | Expired or cancelled |
| `12` | Timed out while waiting |
| `1` | Any other failure |

In CI, set `NODD_KEY` instead of logging in.

## Agents

```sh
claude mcp add nodd -- nodd mcp
```

`nodd mcp` provides `notify`, `ask`, and `check_answer`. A model can ignore MCP answers, so use the hook to enforce approval:

```json
{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "nodd hook", "timeout": 900}]}]}}
```

## GitHub Actions

See [blipdrop/nodd-action](https://github.com/blipdrop/nodd-action).

## API

See [spec/openapi.yaml](spec/openapi.yaml).

## Links

- [Website](https://www.blipdrop.net/nodd/en/)
- [Privacy Policy](https://www.blipdrop.net/nodd/en/privacy/)
- [Terms of Use](https://www.blipdrop.net/nodd/en/terms/)
- Support: support@blipdrop.net

## License

Apache-2.0
