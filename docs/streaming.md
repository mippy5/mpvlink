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

Everyone watching you sees and hears what your mpv plays: pausing, seeking and
changing speed all carry over, and the subtitles you see are drawn into the
video, styled as in your mpv. The audio is the track you're listening to (switch tracks, or turn audio
off, and your viewers follow).
To stop, close the video or click someone else's name.

Streaming needs **ffmpeg** (it encodes the video). mpvlink looks for it on your
PATH, and on macOS also in Homebrew's usual locations. To use a different one,
open the menu (`Ctrl+j`), click **Settings** (or press `Ctrl+,`) and enter its
path under **ffmpeg**; the page shows which ffmpeg is in use. Or set it in
`mpv.conf`, which overrides the saved setting:

```
script-opts-append=mpvlink-ffmpeg=/path/to/ffmpeg
```

Hardware encoding is used when available (VideoToolbox on macOS; NVENC or
Quick Sync on Windows/Linux), else x264. The stream is at most 1080p.

The video bitrate is 6000 kbit/s by default. To change it, open **Settings**,
then **Advanced**, and pick one of these:

- **Match source:** the bitrate of the video you play. A video larger than
  1080p is sent at 1080p, so its bitrate is scaled down to match (e.g. a 4K
  file at 40 Mbit/s streams at 10). Results are kept between 500 and 50,000
  kbit/s.
- **× Source:** the source's bitrate times a number, e.g. 1.5. Values above
  1 make up for H.264 needing more bits than HEVC or AV1 for the same
  quality.
- **Custom:** a fixed bitrate in kbit/s.

A new bitrate applies to your stream right away. The source's bitrate comes
from the file (the video stream's own figure, else the whole file's); if
ffmpeg can't tell, the default is used. In `mpv.conf` the same choices are
`script-opts-append=mpvlink-kbps=4000`, `=source` or `=1.5x`.
Audio is Opus at 128 kbit/s stereo; it needs an ffmpeg with libopus (most
builds have it), otherwise only video is streamed.

Local files and `http(s)://` links can be streamed. Things mpv opens through
other means (like `ytdl://` or YouTube links) can't yet.

## How it stays in sync

ffmpeg encodes ahead of playback, each frame carries its position in the file,
and a frame is sent when your mpv's playback reaches it. Viewers see your
playback a network trip behind (one or two frames in local tests). A seek
restarts ffmpeg at the new position, which shows as a short freeze for viewers.

On the viewer's side, video is shown the moment it arrives, and audio plays
alongside on a small buffer of its own: neither waits for the other, so a
network hiccup on one doesn't stall the other. To keep them together anyway,
audio is sent about 100 ms ahead, which is roughly what the viewer buffers.
Lip sync is therefore close but not frame-exact (expect within a few tens of
milliseconds). If a viewer's audio stops arriving, they hear silence while the
picture carries on.

## Current limitations

- **Subtitles are part of the picture.** The track you have on (from the
  file or a separate subtitle file) is drawn into the video by ffmpeg, with
  libass for text formats like ASS and SRT, so signs and styling survive.
  Viewers can't turn them off. Your subtitle delay and style settings
  (`sub-font`, `sub-font-size`, `sub-scale`, `sub-pos`, `sub-color`,
  `sub-border-style`, `sub-ass-override`, `sub-ass-style-overrides` and so
  on) apply as in your mpv. Changing any of them, or hiding or switching
  subtitles, restarts your stream briefly. Secondary subtitles aren't drawn.
- **Close to mpv, not identical, in a few cases:** with `sub-ass-override`
  set to `scale`, mpv guesses which ASS lines are dialogue to scale; here
  that's every bottom-aligned style. `sub-ass-override=strip` is treated
  like `force` (ASS override tags are kept). `sub-scale` on image subtitles
  scales the whole subtitle layer about the usual subtitle line position,
  where mpv scales each bitmap about its own center.
- **When text subtitles can't be drawn** (an ffmpeg built without libass, or
  streaming an http(s) link, where ffmpeg would have to download the whole
  file first), viewers get the subtitle line as plain text instead.
- **New viewers** start at the next keyframe, which comes every second, and a
  viewer recovers from packet loss the same way.
- **HDR videos** are converted to 8-bit without tone mapping, so they look
  washed out.
- **What you add in mpv** (shaders, filters, your own scaling) isn't part of
  the stream; viewers' mpv applies their own settings.
