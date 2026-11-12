# mpvlink on Windows

The plugin (`mpvlink.dll`), the signaling server (`mpvlink-signal.exe`) and
the test publisher (`mpvlink-send.exe`) all build for 64-bit Windows 10 and
later. They're cross-compiled from macOS or Linux; there's no native Windows
build setup.

## Build

One-time setup:

```sh
brew install mingw-w64                  # Debian/Ubuntu: sudo apt install mingw-w64
rustup target add x86_64-pc-windows-gnu
```

Then:

```sh
make windows                  # everything -> dist/windows-x86_64/
make windows PKG=mpvlink      # just the plugin
```

The outputs only need DLLs that come with Windows: the C/C++ runtimes are
linked in statically, so there's nothing else to ship.

## Install the plugin

You need **mpv 0.37 or newer** (earlier Windows builds can't load C plugins).
The usual Windows builds linked from [mpv.io](https://mpv.io/installation/)
(shinchiro, zhongfly) qualify.

Copy `mpvlink.dll` into mpv's `scripts` folder, creating it if needed:

- installed mpv: `%APPDATA%\mpv\scripts\mpvlink.dll`
- portable mpv (a `portable_config` folder next to `mpv.exe`):
  `portable_config\scripts\mpvlink.dll`

Or try it without installing:

```bat
mpv --idle=yes --force-window=yes --script=C:\path\to\mpvlink.dll
```

Everything works as on macOS: `Ctrl+j` opens the menu, `Ctrl+,` opens
Settings, and `Ctrl+V` pastes. Your display name and server setting are saved in
`%LOCALAPPDATA%\mpv\mpvlink\profile`.

The first time you join a room, Windows Defender Firewall may ask whether mpv
may communicate on the network. Allow it (at least on private networks), or
video may not arrive when the publisher is on another network.

## Running the server or test publisher on Windows

Both work from a Command Prompt or PowerShell, with the same options as
elsewhere:

```bat
mpvlink-signal.exe --listen 0.0.0.0:8787
mpvlink-send.exe --server ws://192.168.1.10:8787 --room TEST42
```

For a public server, the [deployment guide](deploy.md) (Linux + Caddy) is
still the recommended setup.

## What's been tested

- The cross-compiled unit tests (all 43) pass under Wine.
- Under Wine, the Windows `mpvlink-signal.exe` served native macOS clients,
  and the Windows `mpvlink-send.exe` joined rooms through it.
- The plugin exports exactly what mpv's Windows loader looks for:
  `mpv_open_cplugin` plus a `pfn_mpv_*` pointer for each mpv function it uses.

Not yet tested on real Windows: the plugin running in mpv, and video over
WebRTC. Wine can't test video, because its sockets reject an option the UDP
layer (quinn-udp) sets up, so WebRTC fails with
`Invalid argument (os error 10022)`. That's a Wine limitation; real Windows
supports the option.
