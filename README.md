# mpvlink

Watch together in [mpv](https://mpv.io), with very low latency. mpvlink is an
mpv plugin for streaming video to friends over WebRTC. Pick a display name,
create or join a room with a code, and anyone in the room can stream what their 
mpv is playing while the others watch it live.

- **Low latency:** video is shown the moment it arrives. On a local network
  viewers trail the streamer by one or two frames.
- **Several streamers per room:** a member list shows who's streaming; click a
  name to switch to their stream.
- **Stream what you're watching:** with yourself selected, drag a video into
  mpv. Pausing, seeking and speed changes carry over to your viewers, along
  with audio and subtitles.
- **macOS, Linux and Windows.**

## How it works

- The plugin (`mpvlink.so` / `mpvlink.dll`) is a native mpv plugin
  written in Rust. It draws its menus with mpv's on-screen display and feeds
  received video and audio to mpv through a custom `mpvlink://` protocol.
- To stream, it runs ffmpeg on the file mpv is playing and sends each
  frame when mpv's own playback reaches it, so viewers see what you see.
- The **signaling server** (`mpvlink-signal`) keeps track of rooms and passes
  connection details between people.

## Getting started

### 1. Get the plugin

Download it for your platform from
[Releases](https://github.com/mippy5/mpvlink/releases), or build it (see
[Building](#building)). Then put it in mpv's `scripts` folder:

| Platform | File | Scripts folder |
|---|---|---|
| macOS | `mpvlink.so` | `~/.config/mpv/scripts/` |
| Linux | `mpvlink.so` | `~/.config/mpv/scripts/` |
| Windows | `mpvlink.dll` | `%APPDATA%\mpv\scripts\` (or `portable_config\scripts\`) |

mpv 0.37 or newer is needed on Windows (see [docs/windows.md](docs/windows.md)).
On macOS, a downloaded plugin may be blocked until you run
`xattr -d com.apple.quarantine ~/.config/mpv/scripts/mpvlink.so`.

To stream your own videos you also need ffmpeg (with libopus for audio and
libass for subtitles, which most builds include).

### 2. Point it at a signaling server

Everyone in a room needs the same server. Run your own on a small Linux
server: [docs/deploy.md](docs/deploy.md) walks through it, including HTTPS
with a domain or a bare IP address.

Then in mpv press `Ctrl+j`, click **Settings** (or `Ctrl+,`), enter the
server address (e.g. `wss://signal.example.com`) and save.

### 3. Join a room

Press `Ctrl+j`, enter a display name, then either type a room code and press
**Join**, or click **Create room** (the new code is copied to your clipboard
to share).

### Keys

| Key | Action |
|---|---|
| `Ctrl+j` | Open or close the menu |
| `Ctrl+,` | Settings (in the menu) |

For your own `input.conf`: `script-binding mpvlink/toggle-menu` and
`script-binding mpvlink/settings`.

### Options

Settings saved in the Settings page can be overridden with `--script-opts`
(or `script-opts-append=` in `mpv.conf`):

| Option | Meaning |
|---|---|
| `mpvlink-server=wss://host` | Signaling server (default: saved setting, else `ws://127.0.0.1:8787`) |
| `mpvlink-ffmpeg=/path/to/ffmpeg` | ffmpeg for streaming (default: saved setting, else found automatically) |
| `mpvlink-kbps=6000` | Bitrate of your stream |
| `mpvlink-stun=stun:host:port` | STUN server, or `no` for none (default: Google's public one) |

## Limitations

- No TURN relay yet, so two people behind strict NATs or firewalls may not be
  able to connect directly (the signaling works, but no video arrives).
- Streams are H.264 up to 1080p; HDR is converted without tone mapping.
- Subtitles are burned into the video, with your mpv subtitle delay and style
  settings (viewers can't turn them off).
- Only files and `http(s)://` links can be streamed, not YouTube links, and
  not window or screen capture (planned).

## Building

Requires Rust (stable). From the repository root:

```sh
make build        # the plugin: target/release/mpvlink.so
make install      # ...and copy it to ~/.config/mpv/scripts/
make test         # run the tests
```

To try it out locally with no server setup:

```sh
make signal                 # signaling server on ws://127.0.0.1:8787
make run                    # mpv with the plugin; join a room from the menu
make send ROOM=ABCD12       # stream a test pattern into room ABCD12
```

Other platforms, cross-compiled from macOS or Linux:

```sh
make linux                  # dist/linux-x86_64/   (one-time toolchain setup: see Makefile)
make linux PKG=mpvlink-signal
make windows                # dist/windows-x86_64/ (see docs/windows.md)
```

Pushing to GitHub runs [.github/workflows/build.yml](.github/workflows/build.yml),
which builds the plugin for every platform and the server for Linux; pushing a
`v*` tag also publishes a release.

## License

[MIT](LICENSE)
