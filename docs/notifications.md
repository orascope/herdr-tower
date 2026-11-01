# Notifications and Discord

[← README](../README.md)

`~/.config/herdr-tower/config.json`, edited by hand or with `herdr-tower config`:

```json
{
  "discord_webhook_url": "https://discord.com/api/webhooks/...",
  "notify_webhook_url": "https://discord.com/api/webhooks/...",
  "notify_macos": true,
  "notify_discord": false
}
```

- One notification at the moment an agent **finishes (done)** or starts **waiting
  (wait)**, and never for a heartbeat — a macOS notification, plus a one-liner to
  Discord if that is switched on
- `notify_webhook_url` (optional): sends those one-liners to a separate channel.
  Left empty, they go to the card's webhook. Separating them keeps the dashboard
  channel down to a single card with nothing to scroll
- `herdr-tower card` keeps upserting one Discord message with the agent states.
  The resident supervisor refreshes it every five minutes by itself, and a hook
  refreshes it on each state change — nothing needs scheduling. `daemon install`
  removes a `dev.herdr.tower.card` LaunchAgent left from the days that took one
  (only the plain `herdr-tower card` job; anything else in it keeps it yours)
- A run whose `HERDR_TOWER_STATE_DIR` names a store other than the default one
  (a test, a sandbox) posts neither the card nor the one-liners, unless
  `HERDR_TOWER_CONFIG` names a config of its own too. The card's message ID
  lives in the store, so a sandbox would otherwise post a second card to your
  real channel and orphan it
- `preview_interval` (optional): how often the TUI preview refreshes, as a
  duration string (`"500ms"`, `"2s"`). Default `1s`, floor 200ms (anything lower
  is raised to it). Independent of the two-second list poll — this moves the
  preview alone. Applied on the next TUI start
- `wait_renotify` (optional): how long a session may sit in `wait` before it is
  announced once more, as a duration string. Default `"15m"`, floor one minute,
  `"off"` turns it off. One reminder per wait — a second nudge rescues a missed
  first one; a stream of them would be muted. The resident supervisor sends it,
  so it works with the tower closed
- `id_labels` (optional): puts each herdr pane's ID in front of its name
  (`[w2:p1] name`; tabs keep plain names). Default `true`; `false` touches no name
  ([Pane IDs in their names](herdr.md#pane-ids-in-their-names))
- `update_check` (optional): whether the supervisor asks GitHub's public
  releases API for the newest tag, every six hours (hourly after a failed
  request). Default `true`. The tower's header ends in a
  `★ github.com/orascope/herdr-tower ↗` link — a click opens it in the browser —
  and in `v… available ↗` once a release newer than the supervisor is out.
  `false` sends nothing and keeps the plain link

`herdr-tower config` changes these without opening the file:

```bash
herdr-tower config keys                        # every setting: its type, when it applies, what it does
herdr-tower config                             # the file as it is (webhook URLs shown as <hidden>)
herdr-tower config get wait_renotify
herdr-tower config set wait_renotify 20m       # also: true|false|on|off, 9333, 500ms, a,b,c, {"id":"label"}
herdr-tower config set mcp.public_url https://mac.tail1234.ts.net
herdr-tower config unset dock_keep             # back to the default
```

Only known settings can be set, each value is parsed by the setting's type, and
nothing is written unless the result still loads. A file that does not load is
replaced by the defaults at runtime, webhooks included, so a typo must never reach
it. Keys a newer version wrote are kept. A new file is `0600`, and an existing one
keeps its mode. Through the MCP `tower` tool the same command works from a phone
(`["config","set","wait_renotify","off"]`).
