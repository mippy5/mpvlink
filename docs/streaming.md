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

While a video plays, the list hides like mpv's own controls: it shows when you
move the mouse and hides once the mouse is still (after the controls' delay,
`hidetimeout` in `osc.conf`, 500 ms by default) or leaves the window. It stays
while the mouse is over it, when nothing is playing, and when the controls are
set to always show. When someone joins, leaves, or starts or stops streaming,
it shows for a few seconds.

## Streaming a video

1. Click your own name, if the arrow isn't on you already ("Drop a video here
   to stream it" appears under the list).
2. Drag a video into the mpv window, or open it any other way.

Everyone watching you sees and hears what your mpv plays: pausing, seeking and
changing speed all carry over, and the subtitles you see are drawn into the
video, styled as in your mpv (or, if you choose, viewers get every subtitle
track to pick from: see below). The audio is the track you're listening to (switch tracks, or turn audio
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

## Subtitles for viewers

By default your subtitles are drawn into the picture, exactly as you see
them. Viewers can't change or turn them off.

Instead, **Settings → Advanced → Subtitles for viewers → As tracks** (or
`mpvlink-subtitles=tracks`) sends viewers every subtitle track of the file you
stream, plus the fonts attached to it, so styled (ASS) subtitles look as
meant. Each viewer then picks a track, or none, with mpv's usual keys (`j`,
`J`, `v`) or menus, and their own style settings (`sub-scale`, `sub-pos`, …)
and subtitle delay apply. The track you're showing is the one they start with.

- Text tracks (ASS, SRT, WebVTT) and image tracks (PGS) are sent; DVD and DVB
  image subtitles aren't. Subtitle files you loaded separately go too.
- The tracks are extracted when you start a video (ffmpeg reads the whole file
  once, typically a second or two) and sent peer to peer, paced so the video
  isn't disturbed. A viewer's stream reloads once when they arrive.
- For an http(s) link, only separately loaded subtitle files are sent (the
  file's own tracks would need the whole file downloaded).
- Viewers' mpv plays the stream with timestamps then, so it can time the
  tracks; mpvlink keeps their `sub-delay` lined up with your playback (your
  seeks, pauses and speed changes included) while keeping any delay they set
  themselves.

Changing this setting restarts your stream.

## How it stays in sync

ffmpeg encodes ahead of playback, each frame carries its position in the file,
and a frame is sent when your mpv's playback reaches it. Viewers see your
playback a network trip behind (one or two frames in local tests). A seek
restarts ffmpeg at the new position, which shows as a short freeze for viewers.

Frames are sent on a smoothed version of mpv's clock. mpv's playback position
only moves when it shows a frame, unevenly at 24 fps on a 60 Hz screen, so
following it directly made the stream itself uneven.

On the viewer's side, audio plays alongside the video on a small buffer of its
own: neither waits for the other, so a network hiccup on one doesn't stall the
other. To keep them together anyway, audio is sent about 100 ms ahead, which is
roughly what the viewer buffers. Lip sync is therefore close but not
frame-exact (expect within a few tens of milliseconds). If a viewer's audio
stops arriving, they hear silence while the picture carries on.

### Smooth playback

Over the internet, frames arrive unevenly, and shown the moment they arrive,
that unevenness looks like small stutters. So each frame is held until its
time: its timestamp plus a short delay that covers the slowest recent arrivals.
Audio is held back by the same amount, keeping lip sync.

Choose how much in **Settings → Advanced → Smooth playback** (applies right
away):

- **Auto** (default): just enough delay for the jitter measured over the last
  10 seconds. Typically 10–20 ms on a local network, more on a rough
  connection (up to 300 ms).
- **100 ms** / **250 ms**: a fixed delay, for connections with occasional big
  hiccups.
- **Off:** frames are shown the moment they arrive, for the least delay.

The same choices in `mpv.conf`: `script-opts-append=mpvlink-smoothing=off`,
`=auto` or `=100`.

To see what's going on, run mpv from a terminal. Every 10 seconds the
streamer logs how evenly frames go out, and each viewer logs how evenly they
arrive, the delay in use, and any lost packets:

```
[mpvlink] sending: 240 frames, timing spread median 2 / 98% 3 / max 4 ms
[mpvlink] receiving: 240 frames, timing spread median 21 / 98% 40 / max 42 ms; smoothing auto: delay 49 ms, 0 late; 0 losses, 0 out of order
```

"Timing spread" is how much later than the earliest frame each one was, after
allowing for their timestamps. "Late" frames missed their time and were shown
on arrival (a visible hitch). A loss makes the picture freeze until the next
keyframe (at most a second).

## Statistics panel

For tracking down problems, **Settings → Advanced → Statistics panel** (or
`mpvlink-stats=yes`) adds a panel in the top-right corner. It shows and hides
with the member list.

At the top it shows what's playing for AniList tracking (see
[tracking.md](tracking.md)): the show, the episode and how the list update
stands.

While watching, it shows the network round trip (RTT), how unevenly frames
arrive (the spread that smoothing has to cover), the incoming bitrate and frame
rate, lost and out-of-order packets, the smoothing delay and late frames, and
an estimate of the total delay (half the RTT plus smoothing; decoding adds a
little more). Its graphs show each frame's arrival over the last few seconds,
with the smoothing delay as a yellow line and late frames in red, and the
incoming bitrate over the last minute.

While streaming, it shows the encoder, the target and encoded bitrates, the
total upload, how evenly frames go out, and each viewer's RTT and the share of
packets they report lost, with graphs of send timing and upload.

Bitrates in the text are averages over the last 5 seconds; the graphs show
each second.

## Current limitations

- **Drawn-in subtitles** (the default; see "Subtitles for viewers") are part
  of the picture. The track you have on (from the
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
