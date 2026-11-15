# PcVolumeControl protocol, version 7

This is the canonical description of the protocol between the PcVolumeControl
host ("server", running on the PC) and the phone apps ("clients"). It
supersedes the "Client to Server Messages" section of the old
`PcVolumeControlWindows` README, whose examples use a stale `version` field.

## Transport and framing

- Raw TCP. The default port is 3000.
- Each message is one JSON object encoded as UTF-8 and terminated by `\n`. A
  trailing `\r` before the `\n` is tolerated.
- Blank lines from a client are ignored.
- A client line longer than 64 KiB (excluding the terminator) is malformed.

## Server to client: full state

On connect, the server sends the current full state immediately. After that it
sends the full state again to **every** connected client whenever anything
changes: master volume or mute, a session's volume or mute, a session appearing
or disappearing, a device appearing or disappearing, or the default device
changing. Bursts of changes are coalesced into one message (about 15 ms).

The server never replies directly to a client message. A successful update
changes the audio system, which triggers a broadcast like any other change. If
an update cannot be applied (an unknown device id, a session that has just
gone away), the server re-sends the current full state to every client so the
sender can drop its optimistic change. The server may skip a broadcast when the
state has not actually changed.

```json
{
  "protocolVersion": 7,
  "applicationVersion": "v10",
  "deviceIds": {
    "0f4090a9-dee2-4563-ba29-0ad6b93d9e22": "Speakers (Realtek High Definition Audio)",
    "c98e4030-926b-4e62-8c83-1c529574cc51": "Headset (5- USB Audio Device)"
  },
  "defaultDevice": {
    "deviceId": "0f4090a9-dee2-4563-ba29-0ad6b93d9e22",
    "name": "Speakers (Realtek High Definition Audio)",
    "masterVolume": 80.0,
    "masterMuted": false,
    "sessions": [
      {
        "name": "Google Chrome",
        "id": "{0.0.0.00000000}.{0f4090a9-dee2-4563-ba29-0ad6b93d9e22}|\\Device\\HarddiskVolume2\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe%b{00000000-0000-0000-0000-000000000000}",
        "volume": 35.0,
        "muted": false
      }
    ]
  }
}
```

| Field | Type | Notes |
| --- | --- | --- |
| `protocolVersion` | integer | Always `7`. Clients should disconnect on a mismatch. |
| `applicationVersion` | string | Server build, informational. |
| `deviceIds` | object | Active playback devices only, device id to friendly name. |
| `defaultDevice.deviceId` | string | Id of the current default playback device. |
| `defaultDevice.name` | string | Its friendly name. |
| `defaultDevice.masterVolume` | number | 0.0 to 100.0. |
| `defaultDevice.masterMuted` | bool | |
| `defaultDevice.sessions` | array | Audio sessions on the default device. System sounds are excluded. |
| `sessions[].name` | string | Display name, falling back to the process name. |
| `sessions[].id` | string | Opaque to clients. |
| `sessions[].volume` | number | 0.0 to 100.0. |
| `sessions[].muted` | bool | |

All fields are always present. Clients must ignore fields they do not know.
The server sends nothing while there is no default playback device.

### Ids

Device and session ids are opaque strings, but every session id must contain
the id of the device it belongs to as a substring. The iOS client relies on
this: it finds the `deviceId` for a session update by searching the session id
for a key of `deviceIds`, and falls back to sending the session id itself. On
Windows, Core Audio session instance identifiers already have this shape.

## Client to server: updates

A client message is a partial full state. `protocolVersion` is required and
must be `7` (an integral float such as `7.0` is accepted). Unknown fields are
ignored, and `null` is treated as absent.

There are three kinds of update. They are mutually exclusive and the server
checks them in this order, applying only the first that matches:

1. **Change the default device.** `defaultDevice.deviceId` differs from the
   current default device. The server makes that device the default (for
   playback and communications) and ignores everything else in the message. An
   unknown id does nothing.
2. **Change master volume or mute.** `defaultDevice.deviceId` equals the
   current default and `masterVolume` or `masterMuted` (or both) is present.
   The server applies whichever is present and ignores `sessions`.
3. **Change sessions.** Otherwise, for each session on the default device whose
   `id` matches an entry in `defaultDevice.sessions`, the server sets that
   session's volume **and** mute from the first matching entry. Entries for
   unknown sessions are ignored.

A message without `defaultDevice`, or without `defaultDevice.deviceId`, does
nothing. While there is no default device, any `deviceId` is treated as kind 1.
Volumes outside 0 to 100 are clamped.

Updates are judged against the last full state the server sent, which is the
state the client is acting on.

Change the default device:

```json
{"protocolVersion":7,"defaultDevice":{"deviceId":"c98e4030-926b-4e62-8c83-1c529574cc51"}}
```

Change master volume and mute (either field may be omitted):

```json
{"protocolVersion":7,"defaultDevice":{"deviceId":"0f4090a9-dee2-4563-ba29-0ad6b93d9e22","masterMuted":false,"masterVolume":64.0}}
```

Change a session. `id`, `volume` and `muted` are all required in each entry;
`name` is optional and ignored:

```json
{"protocolVersion":7,"defaultDevice":{"deviceId":"0f4090a9-dee2-4563-ba29-0ad6b93d9e22","sessions":[{"name":"Google Chrome","id":"{0.0.0.00000000}.{0f4090a9-dee2-4563-ba29-0ad6b93d9e22}|...","volume":29.0,"muted":true}]}}
```

### Errors

The server closes the client's connection, without sending an error, when a
line:

- is not valid JSON, is not a JSON object, or has a field of the wrong type
  (including a session entry missing `id`, `volume` or `muted`);
- is longer than the line limit;
- has a missing or different `protocolVersion`. A message using the stale
  `version` field is rejected for this reason.

## Discovery (mDNS / DNS-SD)

While the listener is running the server advertises:

| Item | Value |
| --- | --- |
| Service type | `_pcvolumecontrol._tcp.local.` |
| Instance name | `{hostname}-pcvolumecontrol` (renamed by the mDNS stack on conflict) |
| Host | `{hostname}.local.` |
| Port | the TCP listening port |
| TXT `version` | `1` |
| TXT `protocol` | `7` |

The advertisement is withdrawn when the server stops. The Android client
ignores services whose `protocol` TXT record is not its own protocol version;
the iOS client uses the resolved host and port.
