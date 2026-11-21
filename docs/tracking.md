# AniList tracking

Like [Taiga](https://taiga.moe), mpvlink can keep your
[AniList](https://anilist.co) list up to date: when you've watched most of an
episode, the show's progress on your list moves to that episode. It works for
your own files and for streams you watch in a room. Everyone in the room who
has signed in gets their own list updated.

## Signing in

1. Open the menu (`Ctrl+j`), then **Settings**.
2. Click the **AniList** field and press **Get code**. AniList opens in your
   browser and asks you to approve mpvlink.
3. AniList then shows a long code. Copy it, paste it into the field
   (`Cmd+V` / `Ctrl+V`) and press **Save**.

mpvlink checks the code and says who you're signed in as. The code is kept in
mpv's state folder (`mpvlink/anilist`, readable only by you) and lasts about a
year. To sign out, focus the field and press **Sign out**, then **Save**.

## What happens while you watch

When a file starts, mpvlink reads its name the way Taiga does
(`[Group] Title - 05 [1080p].mkv`, `Title S2E05`, `Title 2x05`, …) and looks
the show up on AniList. If it's recognized, a message says what it is, for
example *AniList: Assassination Classroom Second Season · episode 14 of 25*.

Once playback has passed 80% of the episode, and at least half of it has
actually played (skipping to the end doesn't count), your list is updated:

- Not on your list yet, or planned, paused or dropped: it's set to
  **Watching** at this episode.
- Watching: progress moves to this episode (never backwards).
- The last episode: it's set to **Completed**.
- Already completed: left alone (rewatches aren't tracked).

A message shows the update. **Ctrl+Z** undoes it for the next 10 seconds.

If the update can't be sent (no connection, AniList busy or down), a message
says so and it waits in a queue (`mpvlink/anilist-queue` in mpv's state
folder, kept across restarts). Queued updates are sent every two minutes until
they go through, shortly after mpv starts, and right after you sign in, which
is also what a sign-in that expired needs. A message says when a queued update
went through. Updates AniList refuses outright aren't retried.

## Fixing a wrong match

Press **Ctrl+A** while something plays to open **AniList match**. It searches
AniList for the title read from the file name and lists what it finds, with the
current match ticked. Type to change the search and press Enter to run it.
Then click the right show, or highlight it with ↑/↓ and press Enter. Or press
**Don't track** if it isn't anime.

The choice applies to the episode that's playing right away, and to every
file with that title (and season) from then on. When the title can't be found
at all, a message says so and points to Ctrl+A.

### Seasons

AniList has a separate entry for each season. A file that names its season
(`S2`, `2x14`) is matched to that season's entry, by its title ("… 2nd
Season") or by following AniList's sequel links from the first season.

Release groups often number episodes differently: they keep counting across
seasons ("Shingeki no Kyojin - 40" is season 3, episode 3), or number a
special as episode 0. For these, mpvlink uses Taiga's
[anime-relations](https://github.com/erengy/anime-relations) data (public
domain), which maps such episodes to the right entry. A copy is built in, and
once you're signed in a fresh one is downloaded weekly into
`mpvlink/anime-relations.txt` in mpv's state folder. Episodes it doesn't cover
that are counted past a season's end are carried into the next season along
AniList's sequel links (episode 30 of a show whose first season has 22
becomes season 2, episode 8).

Each series is looked up once; mpvlink remembers the match in
`mpvlink/anilist-matches` in mpv's state folder. To change a match, use
Ctrl+A (see above).

## In the statistics panel

With the statistics panel on (Settings → Advanced), its first lines show what's
playing for tracking: the show, then the episode and how its list update
stands. While the episode plays that's "counts at 80% · now 42%, watched 30%";
afterwards it shows whether the list was updated, was already up to date, or
the update is queued. It also says when nothing matched (Ctrl+A to pick).

## In a room

The streamer's mpvlink sends viewers the file's name, its AniList match (if
the streamer is signed in), and the playback position every few seconds. Each
viewer who is signed in has their own list updated by the same rules, counting
only the part of the stream they watched.

This needs a signaling server from this version or later. Older servers
don't relay these messages, and mpvlink doesn't send them to such servers,
so the rest keeps working with them.

## Privacy

Nothing is sent to AniList unless you're signed in. When you are, the title
read from a file's name (never the folder path, only the file's name, or its
folder's name when the file name has no title) is sent to AniList to find
the show. In a room, other members see the name of the file you stream.

## Limitations

- Anime only, and AniList only.
- Files without an episode number (movies) and fractional episodes (recaps
  like 12.5) aren't tracked.
- Unusual numbering that anime-relations doesn't cover may land on the wrong
  season; Ctrl+A fixes it for that title.
