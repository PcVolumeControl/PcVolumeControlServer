# PcVolumeControl protocol, version 8

This is the canonical description of the protocol between the PcVolumeControl
host ("server", running on the PC) and the phone apps ("clients"). It
supersedes the "Client to Server Messages" section of the old
`PcVolumeControlWindows` README, whose examples use a stale `version` field.

Version 8 does not interoperate with version 7; see
[Older versions](#older-versions).

## Transport and framing

- Raw TCP. The default port is 3000.
- Each message is one JSON object encoded as UTF-8 and terminated by `\n`. A
  trailing `\r` before the `\n` is tolerated.
- Blank lines from a client are ignored.
- A client line longer than 64 KiB (excluding the terminator) is malformed.
- Server lines can be much longer (icon replies); see [Limits](#limits).

## Server to client: full state

On connect, the server sends the current full state immediately. After that it
sends the full state again to **every** connected client whenever anything
changes: master volume or mute, a session's volume, mute, `iconId` or
`detail`, a session appearing or disappearing, a device appearing or
disappearing, or the default device changing. Bursts of changes are coalesced
into one message (about 15 ms), and changes to `detail` alone are rate
limited (see [Session details](#session-details)).

The server never replies directly to a client message, except for icon
replies, which go only to the client that asked (see
[Application icons](#application-icons)). A successful update changes the
audio system, which triggers a broadcast like any other change. If an update
cannot be applied (an unknown device id, a session that has just gone away),
the server re-sends the current full state to every client so the sender can
drop its optimistic change. The server may skip a broadcast when the state
has not actually changed.

```json
{
  "protocolVersion": 8,
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
        "muted": false,
        "iconId": "2f0c6a1d9b7e4c35a8d0e6f1b2c3d4e5"
      }
    ]
  }
}
```

| Field | Type | Notes |
| --- | --- | --- |
| `protocolVersion` | integer | Always `8`. Clients disconnect on any other value; see [Older versions](#older-versions). |
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
| `sessions[].iconId` | string | Optional. Names the session's icon; see [Icon ids](#icon-ids). |
| `sessions[].detail` | string | Optional. Tells sessions of one application apart; see [Session details](#session-details). |

All fields except `iconId` and `detail` are always present. Clients must
ignore fields they do not know. The server sends nothing while there is no
default playback device.

### Ids

Device and session ids are opaque strings. Clients compare them for equality
and send them back exactly as received; they must not parse them or derive
one from the other.

## Client to server: updates

A client message is a partial full state. `protocolVersion` is required and
must be `8` (an integral float such as `8.0` is accepted). Unknown fields are
ignored, and `null` is treated as absent. A message may also carry an
`iconRequest` (see [Requesting icons](#requesting-icons-client-to-server)).

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
{"protocolVersion":8,"defaultDevice":{"deviceId":"c98e4030-926b-4e62-8c83-1c529574cc51"}}
```

Change master volume and mute (either field may be omitted):

```json
{"protocolVersion":8,"defaultDevice":{"deviceId":"0f4090a9-dee2-4563-ba29-0ad6b93d9e22","masterMuted":false,"masterVolume":64.0}}
```

Change a session. `id`, `volume` and `muted` are all required in each entry;
`name` is optional and ignored:

```json
{"protocolVersion":8,"defaultDevice":{"deviceId":"0f4090a9-dee2-4563-ba29-0ad6b93d9e22","sessions":[{"name":"Google Chrome","id":"{0.0.0.00000000}.{0f4090a9-dee2-4563-ba29-0ad6b93d9e22}|...","volume":29.0,"muted":true}]}}
```

### Errors

The server closes the client's connection, without sending an error, when a
line:

- is not valid JSON, is not a JSON object, or has a field of the wrong type
  (including a session entry missing `id`, `volume` or `muted`);
- is longer than the line limit;
- has a missing or different `protocolVersion`, `7` included. A message using
  the stale `version` field is rejected for this reason;
- has a malformed `iconRequest`, more than 32 ids in one `iconRequest`, or
  would put more than 64 icon ids outstanding (see [Limits](#limits)).

## Application icons

Sessions may carry an `iconId` that names an image of their application, and
clients fetch the images with an `iconRequest`. Every server answers
`iconRequest`. A server that serves no icons (it has no way to find them, or
its user turned them off) never sends an `iconId` and answers every requested
id with `notFound`.

### Icon ids

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
{"protocolVersion":8,"iconRequest":{"ids":["2f0c6a1d9b7e4c35a8d0e6f1b2c3d4e5","9a8b7c6d5e4f30211203f4e5d6c7b8a9"],"size":96}}
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
to the requesting client only. A client that has not sent an `iconRequest`
never receives an icon reply.

```json
{"protocolVersion":8,"icon":{"id":"2f0c6a1d9b7e4c35a8d0e6f1b2c3d4e5","size":96,"mime":"image/png","data":"iVBORw0KGgoAAAANSUhEUgAA..."}}
{"protocolVersion":8,"icon":{"id":"9a8b7c6d5e4f30211203f4e5d6c7b8a9","error":"notFound"}}
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
must not assume every line is a full state. Ignore lines with neither.

## Session details

Each session object may carry a `detail`: a short, human-readable label that
tells it apart from other sessions of the same application, such as a
browser tab's title or the title of what a player is playing. Firefox on
Linux, for example, opens a stream for each tab that plays, and every one of
them is named `Firefox`:

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
  a label equal to the session's `name`, or a generic one such as
  `Playback`. Many sessions never have one. "Equal" means equal after the
  cleaning below, with zero-width joiners removed and both lower-cased;
  there is no Unicode normalisation, so `Cafe` plus a combining acute
  accent is not equal to `Café`.
- Clients show it when present; there is nothing to request. A server may
  be configured not to send details, and then omits `detail` from every
  session.
- It is display text only, **not an identity**. It may change at any time
  (a tab navigating to another page), appear later or disappear. Session
  `id`, and the `name` where clients key on names, stay the keys for
  anything a client remembers about a session, such as aliases,
  favourites or hidden sessions.
- It is a single line of at most 120 characters (Unicode scalar values),
  so at most 480 bytes of UTF-8. Runs of whitespace, line breaks and tabs
  included, are sent as one space. Other control characters are removed,
  and so are invisible formatting characters that a web page title can
  carry: the soft hyphen (U+00AD), U+200B, the directional marks U+200E
  and U+200F, the bidirectional embeddings and overrides U+202A to U+202E,
  U+2060 to U+2069 (word joiner, invisible operators and the directional
  isolates) and U+FEFF. Zero-width non-joiners and joiners (U+200C,
  U+200D) are kept only between two visible characters, where they shape
  emoji sequences and some scripts. There is no leading or trailing
  whitespace. Longer text is cut on a character boundary and ends with `…`
  (U+2026), still within 120 characters. A cut can still split a grapheme
  cluster, such as an emoji sequence or a letter and its combining mark.
- A change to a detail is broadcast like any other change, except that a
  change in `detail` alone wakes connected clients no sooner than one
  second after the server last broadcast a full state to them. A title
  that changes constantly therefore costs each client at most one line a
  second. Other changes are never delayed by this and carry the latest
  details, and a client that connects meanwhile gets the latest details
  in its first full state.
- **Privacy:** the title of every stream that plays audio, browser tabs
  included, is readable by every client that can reach the server's port;
  the protocol has no authentication. Private-browsing windows are not
  treated specially: their tabs' titles are sent like any other. Servers
  should let users turn details off; the desktop host does with
  `serve_details = false` (see the README).

What the host sends:

- **Linux** (PulseAudio, or PipeWire through `pipewire-pulse`): the
  stream's `media.name`, or its `media.title` when `media.name` is not
  useful. Firefox sets `media.name` to the tab title. Generic labels are
  dropped: `Playback`, `Playback Stream`, `Audio Stream`, `AudioStream`,
  `AudioCallbackDriver`, `ALSA Playback`, `Simple DirectMedia Layer` (all
  ignoring case and extra whitespace) and `cpal-playback-` followed by
  digits. Streams of one application instance with the same name and
  detail (whether or not details are sent) are one session, with the id of
  its lowest-numbered stream; the id changes when that stream ends.
- **Windows**: none. Core Audio gives a session nothing beyond its display
  name, which is already the `name`, and a browser plays all its tabs
  through one session.

## Limits

| Limit | Value |
| --- | --- |
| Ids per `iconRequest` | 32; more closes the connection. |
| Icon ids outstanding per connection | 64. An id is outstanding from the moment the server reads the request until its reply line has been written to the socket. A request that would exceed this closes the connection. |
| `size` | 16 to 256 pixels after clamping; default 64. |
| Encoded PNG | At most 512 KiB; a larger image is answered with `notFound`. |
| Server line length | Icon lines can be several hundred KiB. Clients must accept lines of at least 1 MiB. |
| Client line length | 64 KiB. |
| `detail` | 120 characters, one line. |
| Full states that only change `detail` | At most one a second per client. |

The server answers one client's ids one at a time and encodes at most a
couple of replies ahead of what the socket has accepted, so a client that
stops reading simply stops receiving and is eventually disconnected by the
normal write timeout. The outstanding limit bounds what a client can queue in
the meantime. Exceeding it disconnects rather than dropping ids, so the rule
that every requested id gets exactly one reply always holds: a client never
waits for a reply that will not come.

## Discovery (mDNS / DNS-SD)

While the listener is running the server advertises:

| Item | Value |
| --- | --- |
| Service type | `_pcvolumecontrol._tcp.local.` |
| Instance name | `{hostname}-pcvolumecontrol` (renamed by the mDNS stack on conflict) |
| Host | `{hostname}.local.` |
| Port | the TCP listening port |
| TXT `version` | `1` |
| TXT `protocol` | `8` |

The advertisement is withdrawn when the server stops. A client can use the
`protocol` record to mark servers of another version in its server list
before connecting; the full state's `protocolVersion` is authoritative.

## Older versions

Version 8 does not interoperate with version 7, which the original C#
Windows server (`PcVolumeControlWindows`) and the released phone apps
speak:

- A version 7 app connecting to a version 8 server receives a full state
  whose `protocolVersion` it does not accept. The released Android app shows
  its wrong-version message and disconnects; it also ignores mDNS adverts
  whose `protocol` is not `7`, so it finds a version 8 server only by
  address. The released iOS app does not check the version: it displays the
  state, and the server disconnects it on its first change, because that
  update carries `protocolVersion` 7.
- A version 8 app connecting to a version 7 server reads `protocolVersion` 7
  in the first full state and should tell the user to update the server.

The client rule: on a full state whose `protocolVersion` is not `8`,
disconnect and tell the user which side is older. A lower version means the
server needs updating; a higher one means the app does.

## Guidance for client authors

Icons:

- Request icons without checking anything first: every server answers, with
  `notFound` for ids it has no image for.
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

- Show `detail` whenever a session has one; there is nothing to request.
  Put it below or beside the name in a smaller or dimmer style, on one
  line, ellipsized at the end when it does not fit. A session without one
  shows its name alone.
- Do not use it to identify a session. Keep keying aliases, favourites,
  hidden lists and any saved state on the session `id` (or on `name`); a
  remembered detail would stop matching as soon as the tab moves on.
- Expect it to change often, appear late and disappear. Update the row in
  place: do not reorder or re-sort rows on a detail change, and do not
  animate or announce it.
- A user's alias for a session replaces its name, not its detail. Showing
  the detail under the alias still helps tell same-named rows apart.
