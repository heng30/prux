---
name: ponytail-help
description: >
  Quick reference for ponytail levels and commands. One-shot display.
  Use for /ponytail help, "ponytail help", "how do I use ponytail".
---

# Ponytail Help

Display this reference card when invoked. One-shot, do NOT change mode,
write flag files, or persist anything.

## Levels

| Level | Trigger | What change |
|-------|---------|-------------|
| **Lite** | `/ponytail lite` | Build what was asked, name the smaller option in one line. |
| **Full** | `/ponytail` | The smallest complete change, a check where the logic needs one, and a reply that names what was skipped and any risk. Default. |
| **Ultra** | `/ponytail ultra` | Also questions the request and pushes back before building. |

Level sticks until changed, the extension is disabled, or the session ends.

## Commands

| Command | What it does |
|---------|--------------|
| `/ponytail` | Turn ponytail on at the default level, or report the current level when it is already on. |
| `/ponytail lite\|full\|ultra\|off` | Set the level for this session. |
| `/ponytail status` | Report the session level and the default level. |
| `/ponytail default <lite\|full\|ultra\|off>` | Set the default level for future sessions. |
| `/ponytail review [scope]` | Quality review of a change: bugs, security, load, missing tests, speed, what to cut. Each finding says what goes wrong and how to fix it. |
| `/ponytail audit [scope]` | The same quality review for the whole repo, ranked. |
| `/ponytail debt [marker]` | Harvest `shortcut:` comments into a tracked ledger. |
| `/ponytail gain` | Measured-impact scoreboard: less code, less cost, more speed. |
| `/ponytail help` | This card. |

## Deactivate

`/ponytail off` turns it off for this session. Resume anytime with `/ponytail`.

## Configure the default level

The default level is `full`. Change it for future sessions with
`/ponytail default <level>`, which writes `defaultMode` in
`<agent_dir>/extensions/ponytail.json` (agent directory defaults to `~/.prux`,
override with `PRUX_AGENT_DIR`). Set it to `off` to leave ponytail inactive at
startup and turn it on with `/ponytail` when wanted.
