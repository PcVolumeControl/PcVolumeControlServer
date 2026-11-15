# PcVolumeControl protocol, version 7 (with v8 extensions)

This is the canonical description of the protocol between the PcVolumeControl
host ("server", running on the PC) and the phone apps ("clients"). It
supersedes the "Client to Server Messages" section of the old
`PcVolumeControlWindows` README, whose examples use a stale `version` field.

"Protocol v8" is v7 plus optional, additive extensions (currently: app
icons and session details). It does not change the version number on the wire: see
[Protocol v8 extensions](#protocol-v8-extensions). Everything before that
section is v7 as the shipped apps speak it; the few places the extensions
touch are marked **v8**.

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
changing (**v8**: or a session's `iconId` or `detail`). Bursts of changes are
coalesced into one message (about 15 ms).

The server never replies directly to a client message (**v8**: except for
icon replies, which go only to the client that asked). A successful update
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

**v8:** a server with extensions adds a top-level `features` array and
optional `iconId` and `detail` members to sessions; see
[Protocol v8 extensions](#protocol-v8-extensions).

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
  `version` field is rejected for this reason;
- (**v8**) has a malformed `iconRequest`, more than 32 ids in one
  `iconRequest`, or would put more than 64 icon ids outstanding (see
  [Limits](#limits)).

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
| TXT `features` | **v8**, only when non-empty: comma-separated feature names, e.g. `icons,detail` |

The advertisement is withdrawn when the server stops. The Android client
ignores services whose `protocol` TXT record is not its own protocol version;
the iOS client uses the resolved host and port.

## Protocol v8 extensions

New in v8. Everything in this section is optional for both sides and purely
additive: a v7 client talking to a v8 server, and a v8 client talking to a v7
server, both keep working.

### Compatibility rules

- `protocolVersion` stays `7` in every message in both directions, and the
  mDNS `protocol` TXT record stays `7`. The shipped Android app disconnects
  from any server whose full state has another version and ignores mDNS
  adverts with another `protocol`; the shipped iOS app and the old C# server
  check the version too. Raising the number would cut them off.
- Support is detected through feature names instead (see below). Clients must
  ignore feature names they do not know.
- A server with no extension enabled sends exactly the v7 bytes: no
  `features` key, no `iconId` or `detail` keys, and no `features` TXT
  record.
- New server-to-client line types (icon replies) are only ever sent to the
  client that asked for them. The shipped apps parse every line as a full
  state, and an icon line has no `defaultDevice`, so receiving one would break
  them.
- The old C# server ignores unknown members, so it treats an `iconRequest` as
  an empty update and never replies. Clients must send `iconRequest` only to a
  server whose full state lists `icons` in `features`.

### Feature detection

A server with any extension adds a top-level array to every full state:

```json
{"protocolVersion":7,"applicationVersion":"v10","features":["icons","detail"],"deviceIds":{...},"defaultDevice":{...}}
```

| Field | Type | Notes |
| --- | --- | --- |
| `features` | array of strings | Optional. Absent means no extensions. Unknown entries must be ignored. |

The same list is advertised over mDNS as the TXT record `features`
(comma-separated, e.g. `features=icons,detail`), so a client can show icon
support in its server list before connecting. The full state is
authoritative.

| Feature | Meaning |
| --- | --- |
| `icons` | Sessions may carry `iconId`, and the server answers `iconRequest`. |
| `detail` | Sessions may carry `detail`. |

### Icon ids

With `icons`, each session object may carry an `iconId`:

```json
{"name":"Spotify","id":"...","volume":65.0,"muted":false,"iconId":"2f0c6a1d9b7e4c35a8d0e6f1b2c3d4e5"}
```

| Field | Type | Notes |
| --- | --- | --- |
| `sessions[].iconId` | string | Optional. 1 to 64 characters of `[a-z0-9]`. |

- An icon id is opaque and **content-derived**: the same id always denotes the
  same image, on every connection and every server. Sessions (or apps) with
  the same image share an id. Servers should derive it from a cryptographic
  hash of the source image, for example the first 32 hex digits of its
  SHA-256, so that ids from different servers cannot collide.
- `iconId` is omitted until the server knows it. Servers resolve icons in the
  background, so a session typically appears first without an `iconId` and
  gains it in a later full state, which is broadcast like any other change. A
  session whose icon cannot be found never gets an `iconId`.
- An id names an image, not a size: the server renders it at the size each
  request asks for.

### Requesting icons (client to server)

```json
{"protocolVersion":7,"iconRequest":{"ids":["2f0c6a1d9b7e4c35a8d0e6f1b2c3d4e5","9a8b7c6d5e4f30211203f4e5d6c7b8a9"],"size":96}}
```

| Field | Type | Notes |
| --- | --- | --- |
| `iconRequest.ids` | array of strings | Required. 0 to 32 entries. |
| `iconRequest.size` | number | Optional, default `64`. Desired edge length in pixels; the server rounds it and clamps it to 16..=256. |

- `iconRequest` may be combined with `defaultDevice` in one message. The
  update is applied exactly as without it, and the icons are served as well.
- `null` is treated as absent, as everywhere else.
- Duplicate ids within one request are answered once, at their first
  position.
- An id the server does not know, including one that is not a valid icon id,
  is not an error: it gets a `notFound` reply.
- A malformed `iconRequest` (not an object, `ids` missing or not an array of
  strings, `size` not a number) or one with more than 32 ids is a protocol
  error: the server closes the connection, as for any other malformed input.

### Icon replies (server to client)

For each distinct requested id, in request order, the server sends one line
to the requesting client only:

```json
{"protocolVersion":7,"icon":{"id":"2f0c6a1d9b7e4c35a8d0e6f1b2c3d4e5","size":96,"mime":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAA..."}}
{"protocolVersion":7,"icon":{"id":"9a8b7c6d5e4f30211203f4e5d6c7b8a9","error":"notFound"}}
```

| Field | Type | Notes |
| --- | --- | --- |
| `icon.id` | string | The requested id, verbatim. |
| `icon.size` | integer | Actual edge length in pixels. It may differ from the requested size, for example when the source image is smaller. |
| `icon.mime` | string | Always `image/png` for now. |
| `icon.data` | string | The image in standard base64 (RFC 4648 section 4: `+` and `/`, with `=` padding). |
| `icon.error` | string | Present instead of `size`, `mime` and `data` when there is no image. `notFound` is the only value today; treat any value as "no image for this id". |

Icon replies are interleaved with full states. Replies to one request arrive
in request order, and replies to successive requests arrive in the order the
requests were sent, but full states may arrive in between.

### Telling lines apart

Every server line has `protocolVersion`. Besides that, a line has **exactly
one** of:

- `defaultDevice`: a full state;
- `icon`: an icon reply.

Decide by which key is present; once a client has sent an `iconRequest` it
must not assume every line is a full state. Ignore lines with neither, for
forward compatibility.

### Session details

With `detail`, each session object may carry a `detail`: a short,
human-readable label that tells it apart from other sessions of the same
application, such as a browser tab's title or the title of what a player is
playing. Firefox on Linux, for example, opens one stream per tab, and every
one of them is named `Firefox`:

```json
"sessions": [
  {"name":"Firefox","id":"alsa_output.pci-0000_0e_00.6.analog-stereo|95","volume":100.0,"muted":false,"iconId":"5c1d0e3f9a8b7c6d5e4f30211203f4e5","detail":"Big Buck Bunny - YouTube"},
  {"name":"Firefox","id":"alsa_output.pci-0000_0e_00.6.analog-stereo|136","volume":100.0,"muted":false,"iconId":"5c1d0e3f9a8b7c6d5e4f30211203f4e5","detail":"Radio Paradise - Main Mix"},
  {"name":"Firefox","id":"alsa_output.pci-0000_0e_00.6.analog-stereo|141","volume":100.0,"muted":false,"iconId":"5c1d0e3f9a8b7c6d5e4f30211203f4e5"},
  {"name":"Spotify","id":"alsa_output.pci-0000_0e_00.6.analog-stereo|127","volume":65.0,"muted":false}
]
```

| Field | Type | Notes |
| --- | --- | --- |
| `sessions[].detail` | string | Optional. One line of 1 to 120 characters. |

- `detail` is omitted when there is nothing useful to say: no label at all,
  a label equal to the session's `name` (ignoring case), or a generic one
  such as `Playback`. Many sessions never have one.
- It is display text only, **not an identity**. It may change at any time
  (a tab navigating to another page), appear later or disappear. Session
  `id`, and the `name` where clients key on names, stay the keys for
  anything a client remembers about a session, such as aliases,
  favourites or hidden sessions.
- It is a single line of at most 120 characters (Unicode scalar values),
  so at most 480 bytes of UTF-8. Runs of whitespace, line breaks and tabs
  included, are sent as one space; other control characters are removed;
  there is no leading or trailing whitespace. Longer text is cut on a
  character boundary and ends with `…` (U+2026), still within 120
  characters. A cut can still split a grapheme cluster, such as an emoji
  sequence or a letter and its combining mark.
- A change to a detail is broadcast like any other change. A full state
  whose only change is in `detail` is sent no sooner than one second after
  the previous full state, so a title that changes constantly cannot flood
  clients; other changes are never delayed by this and carry the latest
  details.

What the host sends:

- **Linux** (PulseAudio, or PipeWire through `pipewire-pulse`): the
  stream's `media.name`, or its `media.title` when `media.name` is not
  useful. Firefox sets `media.name` to the tab title. Generic labels are
  dropped: `Playback`, `Playback Stream`, `Audio Stream`, `AudioStream`,
  `AudioCallbackDriver`, `ALSA Playback`, `Simple DirectMedia Layer` (all
  ignoring case and extra whitespace) and `cpal-playback-` followed by
  digits.
- **Windows**: none, although the feature may be advertised. Core Audio
  gives a session nothing beyond its display name, which is already the
  `name`, and a browser plays all its tabs through one session.

### Limits

| Limit | Value |
| --- | --- |
| Ids per `iconRequest` | 32; more closes the connection. |
| Icon ids outstanding per connection | 64. An id is outstanding from the moment the server reads the request until its reply line has been written to the socket. A request that would exceed this closes the connection. |
| `size` | 16 to 256 pixels after clamping; default 64. |
| Encoded PNG | At most 512 KiB; a larger image is answered with `notFound`. |
| Server line length | Icon lines can be several hundred KiB. Clients must accept lines of at least 1 MiB. |
| Client line length | 64 KiB, as in v7. |
| `detail` | 120 characters, one line. |
| Full states that only change `detail` | At most one a second. |

The server answers one client's ids one at a time and encodes at most a
couple of replies ahead of what the socket has accepted, so a client that
stops reading simply stops receiving and is eventually disconnected by the
normal write timeout. The outstanding limit bounds what a client can queue in
the meantime. Exceeding it disconnects rather than dropping ids, so the rule
that every requested id gets exactly one reply always holds: a client never
waits for a reply that will not come.

### Guidance for client authors

Icons:

- Request icons only when `features` contains `icons`.
- Cache images by `iconId` (and the size you asked for) indefinitely, across
  reconnects and across servers. Persisting the cache is encouraged.
- Request only ids you have not cached and have not already requested on this
  connection. When a full state arrives, collect the unknown `iconId`s of all
  its sessions and send them in one request (several, if more than 32). Keep
  at most 64 ids outstanding; wait for replies before requesting more.
- Expect `iconId` to appear after the session does. Show a placeholder (an
  initial, a generic app glyph) until the image is in hand.
- On `notFound`, keep the placeholder and remember the result for the rest of
  the connection; do not re-request the id in a loop. Trying again on a later
  connection is fine.
- Request the size you draw at in physical pixels (48 dp at 3x density is
  144). The reply's `size` may be smaller; scale it.

Session details:

- Show `detail` whenever a session has one, whatever `features` says; there
  is nothing to request. Put it below or beside the name in a smaller or
  dimmer style, on one line, ellipsized at the end when it does not fit. A
  session without one shows its name alone, as in v7.
- Do not use it to identify a session. Keep keying aliases, favourites,
  hidden lists and any saved state on the session `id` (or on `name`, as
  before); a remembered detail would stop matching as soon as the tab moves
  on.
- Expect it to change often, appear late and disappear. Update the row in
  place: do not reorder or re-sort rows on a detail change, and do not
  animate or announce it.
- A user's alias for a session replaces its name, not its detail. Showing
  the detail under the alias still helps tell same-named rows apart.
