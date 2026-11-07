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

## Messages

`nodd send` and `nodd ask` can format the body and attach a small image.

```sh
nodd send "Nightly report" --markdown --body - < report.md

nodd send "Error rate" --image graph.png --image-alt "Errors over the last hour"
```

`--markdown` allows up to 4,000 characters. `--image` takes a JPEG or PNG up to 40 KB.

## Text and secrets

`--input` asks for up to 200 characters of text. The answer goes to standard error as `Note:` and is in `--json` output. It is not encrypted to devices, so never use it for secrets.

```sh
nodd ask "Roll back?" --input "Which version?" --input-required
```

`--secret` asks for a password or other secret in a masked field. The value is sealed to a key that exists only for this run, so the server cannot read it. It is removed from the server when read, or after 10 minutes.

```sh
nodd ask "Production DB password" --secret --env PGPASSWORD -- psql -h db.example.com app
```

With `--env`, the command runs only on approval, and the value goes to the command alone. Without `--env`, `nodd` prints the value to standard output, and it refuses to print to a terminal. Use `--env` with agents so the value never reaches the model.

Sealing protects the value from storage on the server, not from a compromised server, because the public key passes through it. Answering `--input` or `--secret` needs Nodd 1.1 or later.

## Agents

```sh
claude mcp add nodd -- nodd mcp
```

`nodd mcp` provides `notify`, `ask`, and `check_answer`. There is no MCP tool for secrets. A model can ignore MCP answers, so use the hook to enforce approval:

```json
{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "nodd hook", "timeout": 1260}]}]}}
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
