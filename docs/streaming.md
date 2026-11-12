# Watching and streaming in a room

Anyone in a room can stream, several people at once, and everyone chooses
whose stream to watch.

## The member list

While you're in a room, the top-left corner lists everyone in it:

- **▶ arrow:** whose stream you're watching. Click a name to switch.
- **LIVE:** that person is streaming.
- **The arrow on yourself:** you're watching your own playback, and a video you
  open, for example by dragging it into the window, is streamed to the room.

Until you click someone, the arrow follows whoever is streaming (the first to
start, if there are several), and otherwise points at you.

## Streaming a video

1. Click your own name, if the arrow isn't on you already ("Drop a video here
   to stream it" appears under the list).
2. Drag a video into the mpv window, or open it any other way.

Everyone watching you sees what your mpv plays: pausing, seeking and changing
speed all carry over, and the subtitle line you see is shown to them too.
To stop, close the video or click someone else's name.

Streaming needs **ffmpeg** (it encodes the video). mpvlink looks for it on your
PATH, and on macOS also in Homebrew's usual locations. Otherwise, set its
location in `mpv.conf`:

```
script-opts-append=mpvlink-ffmpeg=/path/to/ffmpeg
```

Hardware encoding is used when available (VideoToolbox on macOS; NVENC or
Quick Sync on Windows/Linux), else x264. The stream is at most 1080p, at
6000 kbit/s by default (`script-opts-append=mpvlink-kbps=4000` to change).

Local files and `http(s)://` links can be streamed. Things mpv opens through
other means (like `ytdl://` or YouTube links) can't yet.

## How it stays in sync

ffmpeg encodes ahead of playback, each frame carries its position in the file,
and a frame is sent when your mpv's playback reaches it. Viewers see your
playback a network trip behind (one frame in local tests). A seek restarts
ffmpeg at the new position, which shows as a short freeze for viewers.

## Current limitations

- **No audio yet;** only video is streamed.
- **Subtitles** are sent as plain lines: no positioning or styling (signs,
  karaoke), and no image-based subtitles (Blu-ray PGS).
- **New viewers** start at the next keyframe, which comes every second, and a
  viewer recovers from packet loss the same way.
- **HDR videos** are converted to 8-bit without tone mapping, so they look
  washed out.
- **What you add in mpv** (shaders, filters, your own scaling) isn't part of
  the stream; viewers' mpv applies their own settings.
