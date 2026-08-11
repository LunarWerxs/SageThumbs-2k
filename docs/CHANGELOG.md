# Changelog

All notable user-facing changes to **SageThumbs 2K**. Newest first.

## 1.10.1

### Fixed

- **One-click updates work again.** Since 1.3.3, clicking "Update" downloaded the new
  version, verified it, and then failed with a message wrongly claiming you needed an
  administrator account. The real cause was on our side: the app kept its own grip on the
  downloaded installer in a way that stopped Windows from starting it. If you are on any
  version from 1.3.3 through 1.10.0, this one update has to be downloaded by hand from the
  releases page; after that, one-click updating works from then on. The whole update path
  now runs for real - install, then upgrade itself in place - on every code change and on
  every release before it is published, so it cannot silently break like this again.

## 1.10.0

### Added

- **Press Space to preview inside [Everything](https://www.voidtools.com/).** Click a result in
  voidtools' search window and tap Space, and you get the same instant preview you get in a
  folder, following you as you arrow down the hits. Everything 1.5 and up, installed or
  portable. Asked for on the voidtools forum, where the answer was that support had to be added
  on our side; it has been. While your cursor is still in Everything's search box, Space stays a
  space, because that is what it should do.

- **Press Space to preview inside any app's Open/Save dialog.** Picking a photo to attach or a
  file to upload, and not sure which one it is? Click it in the dialog and tap Space to see it
  full size, instead of cancelling out to go and look. It works even in Documents, Desktop and
  Downloads, where the dialog does not otherwise tell anyone what it is pointing at. Space keeps
  its normal meaning while the cursor is in the file-name box, so the preview only ever appears
  once you have clicked into the file list. 64-bit applications.

- **Choose which tool the screenshot editor starts in**, under Settings, Screenshots. It now
  opens holding the **arrow** instead of the rectangle, because pointing at the thing you just
  captured is usually why you are annotating it, and you can set it to any of the ten drawing
  tools if you would rather start somewhere else.

- **A FAQ**, at [docs/FAQ.md](FAQ.md), linked from the top of the README. Thumbnails not
  appearing, files stored online-only, antivirus warnings, what the portable zip can and cannot
  do, and the limits of the Space preview, all answered in one place instead of scattered across
  issues.

### Known limitation

- **The Space preview cannot work over a window running as administrator.** Windows deliberately
  withholds keystrokes typed into an elevated window from ordinary programs, so the keypress
  never reaches us. If Everything is set to run as administrator, turn that off (Tools, Options,
  General) and use its service instead. The FAQ has the full steps.

## 1.9.0

### Added

- **A search box that finds any setting, on any page.** There are nine pages and forty-odd
  switches now, and "where is that option" had become the slowest part of using Settings. Type
  into the box at the top right and it lists every match as "Page ▸ Setting"; click one and it
  opens that page and highlights the control for you. It searches the setting names AND their
  descriptions, so "cover" finds the film poster option even though the word "cover" is not in
  its label.

- **An Appearance page**, holding everything about how a thumbnail LOOKS: the format badge and
  its colour, the checkerboard behind transparent images, the Windows file-type icon overlay,
  and using a video's cover art. Those switches were previously spread between General and File
  types, where each page mixed "what SageThumbs handles" with "how it draws". General is now
  limits and language; File types is the format list.

- **A dot on the sidebar next to any page where you have changed something from its default.**
  Nine pages is too many to check one at a time when you are trying to remember what you
  changed. The dot clears once you open that page, and stays cleared. It is a pointer to
  somewhere you have not looked, not a badge that nags forever.

### Changed

- **The Right-click menu page is no longer cramped.** The list of menu items that you can tick,
  untick and drag into order moved into its own "Edit menu items..." window, which gives it room
  to be readable instead of squeezing it under everything else on the page.

- **The welcome screen asks a second, plain question.** It offers the two options most people
  want on but that ship off: using a film's cover art as its thumbnail, and skipping scanlation
  credit pages when picking a comic cover. Each is a switch that says exactly what it turns on.
  Both stay off unless you say so, and both live in Settings if you change your mind.

- **Every page now opens with a heading**, and there is more space between the groups within a
  page, so the sections read as sections rather than one long list of switches.

### Fixed

- **Windows opened in the top-left corner of the screen instead of in front of you.** The
  welcome screen, and every other window SageThumbs opens on its own (convert, resize, image
  info, the doctor report, the OCR result, files-to-folder and tags-to-folders), all placed
  themselves in the corner of the primary monitor. They now open centred on the monitor your
  mouse is on, at that monitor's scaling.

- **The search boxes were too tall and their text sat too high in them.** Both the settings-wide
  search and the format filter left a band of dead space under the text. They are shorter now,
  with the text centred.

- **The rounded corners on the search box, its results list and the format list were being cut
  off**, leaving the edges looking bitten. They are drawn smooth now, and the search box no
  longer has its right-hand curve sliced flat by the edge of the panel it sits on.

- **The parts of the app that were still English in every other language now aren't.** Settings
  sync (its heading, its button, and every status line it shows), the tray icon's menu, the
  list of actions you can bind to the custom hotkey, the hints along the bottom of the
  screenshot overlay, the update prompt, and the page the browser lands on after you sign in
  were all hardcoded English, so they stayed English in all 35 translations. That is 44 new
  pieces of text, translated into every language SageThumbs ships.

- **Page descriptions were being cut off in most languages.** The new search box sits in the
  top right of the page header, and the description under the page title stops short of it.
  That left less room than the translations needed, so 161 of them ended in "..." across the
  36 languages, English's own Right-click menu line among them. Every description is shorter
  now, and a release check measures all of them so a future translation cannot quietly
  overflow again. Ten sidebar names were over-long too and were being cut off mid-word, which
  is what truncated the Greek "Data & backup" page title.

- **The file-types table let you drag its columns wider and narrower.** The last column is sized
  to fill the table, so dragging could only ever cut the description short or open a gap, and
  the divider you dragged it by drew as a bright line down the right-hand edge. Dragging is off
  and the line is gone.

## 1.8.5

### Added

- **`st2k doctor` now tells you which video codec a file uses, and whether Windows can actually
  decode it.** This is the answer to "my mkv has no thumbnail" when everything else looks fine,
  and it usually does look fine, because nothing is broken on our side. Windows does not include
  every decoder: HEVC and AV1 are separate Microsoft Store downloads. Without one, no frame can
  be produced no matter how healthy the file, the container or our registration are. The report
  now names the codec, says whether a decoder is installed, and names the exact Store package to
  get. It also reports Media Foundation being missing altogether, which is what an "N" or "KN"
  edition of Windows looks like.

- **Video files with cover art now use it when no frame can be decoded.** Matroska files can
  carry a poster image, and many library rips do. If the codec is one Windows cannot read, that
  poster is still a true picture of the film, so it becomes the thumbnail instead of a blank tile.

### Fixed

- **Some MP4/MOV files reported "no video track" when they plainly had one.** If the first track
  in the file was an odd one (a subtitle, hint or empty track that editing software leaves
  behind), the search gave up there instead of moving on to the next track. Those files fell back
  to a lower-quality early frame, and `doctor` claimed their codec was unidentifiable.

- **Explorer's Details pane could stop showing file information entirely, until it was
  restarted.** Two files whose reads never return, typically OneDrive "online-only"
  placeholders or a network share that dropped, permanently used up both metadata slots. From
  then on, every other file in that Explorer process showed no dimensions, no camera info and no
  audio tags, including perfectly ordinary local files, with nothing to indicate why. The slots
  now expire, so a stuck file costs a short pause instead of the feature.

- **Word, Excel and PowerPoint documents could show the wrong picture as their thumbnail.** An
  unrelated image inside the document could be mistaken for the document's official preview,
  which also meant the real preview was never looked for.

- **A comic archive could lose its cover page** and show the plain zip icon, if it happened to
  contain a file with a name Office documents use.

- **`st2k doctor` no longer reports things that are not true.** Given a file path relative to the
  current folder, it accused Explorer of refusing to produce a thumbnail when Explorer had simply
  never been asked. It suggested ImageMagick as the cause of video failures, which ImageMagick is
  never involved in. And with the size limit set to "no limit", it reported a cap of roughly
  seventeen million megabytes rather than saying "Unlimited".

- **Some Matroska files were read incorrectly.** Files whose internal size fields use the longest
  encoding, which is what ffmpeg writes for nearly every file it produces, were parsed with a
  malformed bit mask, giving nonsensical sizes.

## 1.8.4

### Added

- **The format badge can now be a coloured mark instead of plain letters.** Reading three small
  letters is slower than seeing a colour, so the badge is now a small tinted file mark with the
  format written inside it, and the tint says what KIND of file it is: images, camera RAW,
  ebooks, documents, audio, video and archives each get their own. Ticking "Show format badge on
  thumbnails" gets you the coloured version; untick "Use a coloured icon badge" next to it for
  the old plain text chip.

- **A checkerboard behind transparent thumbnails**, if you want one. Explorer normally shows the
  folder background through a see-through PNG, which is correct but can make a mostly-transparent
  logo hard to see. The new switch on the General page paints the familiar light checkerboard
  behind it instead. Off by default, and separate from the preview windows' own checkerboard,
  because they are genuinely different surfaces.

- **A switch to stop Windows drawing its own file-type icon on top of your thumbnails**
  (Settings ▸ File types). Windows stamps the associated program's icon into the bottom-right
  corner of a thumbnail, exactly where the format badge goes, and when that program has been
  uninstalled it draws a blank page instead. This asks Windows not to draw it for the file types
  SageThumbs handles. Reversible: unticking puts it back.

- **`st2k doctor` now asks the shell for the thumbnail itself**, instead of only proving our own
  decoder works. Those two answers can disagree, and when they do the disagreement IS the
  diagnosis. It also names the folder's cloud-sync provider when the file lives in one, and calls
  out file types whose leftover associations are stamping an icon over the badge.

### Changed

- **The playback bar's icons are now the system's own icon set.** The previous/next, play/pause,
  speaker, repeat and arrow buttons were drawn by hand and looked it, especially next to the title
  bar's buttons. They now come from the same Windows icon font as everything else, so the two rows
  finally look like one family. Muting also swaps the speaker for the proper crossed-out glyph.

## 1.8.3

### Added

- **Video and music you can actually drive from the keyboard.** The Quick preview could only
  ever be poked with the mouse while something was playing. Now the arrow keys seek (hold Ctrl
  for a thirty-second jump, Shift for a one-second nudge), up and down set the volume, `M`
  mutes, `L` toggles repeat, and `K` or `P` pauses. Page Up and Page Down still flip to the
  next file in the folder, so nothing you already do has changed.

- **Previous / next file buttons on the playback bar**, so you can walk through a folder of clips
  without touching the keyboard.

- **A repeat button and a speed button on the playback bar.** Clips used to loop forever with
  no way to stop them, which is the wrong behaviour when you are checking whether a render
  finished. Repeat is now a button you can turn off, and it remembers your choice. Next to it,
  a speed button cycles 0.5x, 1x, 1.25x, 1.5x and 2x for skimming a long recording.

- **A button on the playback bar for what Left/Right do.** Off, which is the default, they seek
  inside the clip. On, they move to the previous and next file instead. Either way the bar's own
  ⏮ ⏭ buttons and Page Up / Page Down always switch files, so nothing is ever out of reach.

- **Every button on the playback bar now names itself when you hover it**, the way the buttons in
  the title bar always have.

- **"Load web images" is now a button in the preview itself**, on the title bar, and it only shows
  up on documents that actually reference web-hosted images. It used to be a checkbox in Settings,
  which is not where anyone looks when they are staring at a page full of grey labelled chips and
  wondering whether the real pictures can be shown.

- **Find in a document with Ctrl+F.** Works in text files, code, Markdown, and CSV tables. Type
  and it jumps to the first match with a running count of how many there are; Enter or F3 walks
  through them, Shift goes back, and Escape closes the bar. F3 reopens the last search without
  retyping it.

- **Links in a previewed web page now open in your browser.** Clicking a link in a local
  `.html` file used to do nothing at all. Ordinary web links now hand off to your default
  browser; the preview itself still runs with scripts off and no network access of its own.

### Changed

- **Videos open at their real shape.** Every clip used to open into the same wide window, so
  anything filmed on a phone sat as a small strip in the middle of a lot of empty space. The
  window now takes the video's actual dimensions, rotation included.

- **Transparent images sit on a checkerboard.** A white logo on a transparent background looked
  like an empty pane. It now gets the same subtle checkerboard the right-click preview has
  always used, controlled by the same setting. Transparent images are also resized more carefully
  than before, so fine detail no longer breaks up when one is shown smaller than its real size.

- **Files with no extension are recognised by their content.** A picture saved without a file
  extension used to show only an icon; the preview now looks at the file itself. Files with no
  extension but a well-known name (`Makefile`, `Dockerfile`, `.bashrc`, or anything starting
  with a `#!` line) also get syntax colouring now.

- **CSV previews show ten times more rows** (10,000 instead of 1,000) and gained a row-number
  column, so you can find your place in a long file. Pipe-separated `.psv` files are now shown
  as a table too.

### Fixed

- **A video that opens but cannot be decoded no longer shows a permanently blank pane.** It now
  falls back to a still frame, the same as a file the player refuses outright.

- **Archive listings no longer show mojibake for names written by older tools.** Entry names
  that are really UTF-8 but not flagged as such are now read correctly.

- **The preview hotkey no longer occasionally pops a file picker** when pressed immediately
  after switching windows, instead of previewing what you had selected.

- Markdown files that begin with a YAML block (the `---` header used by static site generators
  and note-taking apps) show that block as a tidy code block instead of scrambling it.

- Text files saved as UTF-32 are now decoded properly instead of appearing as gibberish.

- Zooming an image now lands exactly on "fit to window" and "100%" instead of skipping past
  them by a fraction.

## 1.8.2

### Added

- **Press Space on a database file and you can see what's in it.** SQLite files (`.db`,
  `.sqlite`, `.sqlite3` and the rest of that family) now open in the Quick preview like any
  other document: the size and page layout at the top, then a section for each table with its
  real column names and the first rows laid out in a grid, and the full `CREATE` statements at
  the end, syntax-highlighted. The Contents sidebar lists the tables, so a database with thirty
  of them is still one click per table. It stays instant on large files because it reads only
  the pages it needs instead of loading the whole thing, and a long table is cut off with a
  note saying how many rows there really are. Nothing is ever written and no SQL is run against
  your database; the file is read directly, the way the thumbnailer reads a JPEG. Because `.db`
  is such a generic extension, anything that turns out not to be a SQLite file at all (Windows'
  own `Thumbs.db`, for example) previews exactly as it did before.

- **The portable version now offers to switch thumbnails on the first time you run it.**
  1.8.1 made Explorer thumbnails possible without installing anything, but you had to know to
  go and find the switch, and the welcome window still opened with a line saying thumbnails
  were already being added, which is true of the installed build and not of a zip you just
  unpacked. Anyone who took it at its word saw no thumbnails and reasonably concluded the
  thing was broken. The welcome window now says what is actually true of a portable copy and
  offers thumbnails as the first choice on it, alongside Quick preview and the screenshot
  hotkey. It still only ever writes to your own user account, needs no administrator rights,
  and Settings ▸ Advanced turns it back off. Installed builds are unchanged.

### Fixed

- **Wide tables in the Quick preview ran into each other and off the edge of the window.**
  A CSV export with a lot of columns was the worst case: headings printed on top of one
  another, so `username` and `password` came out as one unreadable word, and the values kept
  going past the right-hand edge of the table with no grid around them. Resizing the window
  made it worse rather than better. Three things were wrong at once. A value with no spaces in
  it, like a long API key, could not be wrapped anywhere, so it was drawn at full length
  straight through whatever was next to it. The column widths were allowed to add up to more
  than the window, so the grid lines stopped at the edge while the text carried on past them.
  And nothing stopped a cell from painting outside its own column in the first place. Columns
  are now shared out the way a browser does it, so a narrow column keeps its full width and
  only the genuinely wide ones give way; anything too long to fit wraps onto the next line
  inside its own cell; and every cell is clipped to its column no matter what. Tables in
  Markdown files, in the new database view and in READMEs get the same fix, and a very long
  unbroken word in ordinary text now wraps instead of disappearing off the side.

- **Worth updating for if you keep HEIC or AVIF photos: their thumbnail colours have been
  wrong since 1.3.6, for everybody, not only the person who reported it.** Details below.

- **HEIC, AVIF and JPEG XR thumbnails had their red and blue channels swapped.** Skies came
  out orange, skin came out blue. It only happened when the thumbnail was smaller than the
  picture, which is to say almost always, so if you have iPhone photos in a folder this is
  the bug you were looking at. It has been there since 1.3.6. The resizing step handed the
  pixels back in a different channel order than the one it was given, and we took them at
  their word. Full-size operations were never affected, which is why Convert, Resize and the
  preview pane always looked right while the folder view did not. (issue #9)

- **AVIF colours no longer go wrong when your PC is busy.** Converting a batch of files, or
  anything else that pins your processor, could make the odd thumbnail come out with shifted
  colours while the rest were fine. Reading an AVIF correctly takes us about a third of a
  second of actual work, but we were giving up on it after 20 seconds of waiting, and on a
  fully loaded machine a job that small can sit in the queue for longer than that. We then
  quietly fell back to the Windows decoder, which is the one that gets these files wrong. The
  time limit now counts work done rather than time passed, so a busy machine no longer
  changes the result, while a genuinely stuck file is still stopped as quickly as before.
  (issue #9)

## 1.8.1

### Added

- **The portable version can now do Explorer thumbnails after all.** 1.8.0 said it couldn't.
  That was wrong, and this fixes it. The zip now includes the thumbnail handler, and
  `st2k register` (or the button in Settings -> Advanced) switches it on for your user
  account only - no installer, no administrator rights, nothing written machine-wide. The
  classic right-click menu comes with it. `st2k register --off` undoes it, and you should run
  that before moving or deleting the folder, otherwise Windows is left pointing at a file that
  is no longer there. The Explorer preview pane, the Details pane columns and the Windows 11
  right-click menu still need the installer, because Windows only accepts those registered for
  the whole machine. (issue #13)

## 1.8.0

### Added

- **Optional format badge on thumbnails.** Turn it on and every thumbnail gets a small label
  in its corner saying what the file actually is - PSD, JXL, AVIF, JP2 - so you can tell
  formats apart in a folder where the pictures look alike. Off by default, in Settings ->
  General. Switching it on or off clears the thumbnail cache for you, because Windows stores
  the picture we drew and would otherwise keep showing the old one. (in-app suggestion)

- **A portable version, no installer.** Extract the zip and run it. Nothing is installed, no
  administrator rights are needed, and your settings live in `SageThumbs2K.ini` next to the
  exe rather than in the registry - delete that file and it goes back to storing them the
  normal way. You get the app and the command line tool, so Settings, Convert and Resize,
  quick preview, screenshots, OCR, the colour picker and the folder tools all work. It does
  **not** give you Explorer thumbnails or the right-click menu. (Superseded in 1.8.1, which
  adds both.) The screenshot tool also runs only while the app is open, since a portable copy
  can be moved or unplugged at any time. (issue #13)

- **A "Check for problems" button in Settings, under Advanced.** SageThumbs has had a thorough
  self-check for a while, but only from a command prompt, so in practice nobody ever saw it. It
  now opens in a window you can read and copy, and it tells you in plain words what is wrong and
  how to fix it, whether that is a Windows setting, our registration, or the file itself.

- **`st2k doctor` now catches four more reasons thumbnails go missing**, all of them things
  that look like the app is broken when it isn't. It spots when Windows' "Adjust for best
  performance" setting is on, which switches thumbnails off and quietly turns them back off
  every time you fix it. It tells you when a file lives in OneDrive and hasn't been downloaded
  yet, so there is nothing on your PC to make a picture from. It warns if an old pre-2017
  SageThumbs is still installed, because running *its* uninstaller can strip the registrations
  this one needs. And it reminds you that a folder set to Details, List or Small icons never
  shows thumbnails at all, whatever software you have. Each one comes with the fix.

- **Setup can now restart File Explorer for you at the end**, as a tick box on the last page.
  Installing a thumbnail handler doesn't clear the pictures Windows already remembered, so
  files you had browsed before could keep showing plain icons and make a perfectly good
  install look like it did nothing. Ticking the box clears that and brings Explorer straight
  back. It closes any open Explorer windows, so it is a choice rather than something setup
  does behind your back. Wherever SageThumbs restarts Explorer, it now waits and checks it
  actually came back, and starts it again itself if it didn't, so you can never be left
  looking at an empty desktop.

- **"Restart hotkey service" now sits on the Screenshots page**, next to the hotkey it belongs
  to, with a green or red badge saying whether the helper is actually running. It used to be
  buried under Advanced, which is not where you look when your hotkey has stopped working.

- **The screenshot hotkey now repairs itself if something deletes its startup entry.** Some
  antivirus tools remove it, because a program adding itself to Windows startup looks like
  something malware does, and until now that left the hotkey quietly dead at your next sign-in.
  Opening Settings puts it back.

### Changed

- The PDF page-margin option moved from General to the Ebook/comic tab, where the other
  document settings already live.

- **The reply field on the feedback form is an email address now.** It used to say "email or
  other contact" and accept anything, so people typed their actual message into it and we had
  no way to write back. It is still optional - leave it empty and your feedback is sent
  anyway - but if you do fill it in, it has to be an address that could receive a reply.

## 1.7.5

### Fixed

- **Black-and-white scanned pages could render as a solid black square.** Some JPEG 2000
  scans - the kind archive.org uses for book and map pages - store the picture as numbered
  colours rather than as brightness, and a blank white page is stored as "colour number 0".
  Our new decoder was treating that number as a brightness value, so the whitest page came
  out the darkest. It now reads the colour table the file provides. (issue #11)

- **The preview pane could stop refreshing after it had been sitting idle.** Come back to a
  folder after a while, click a different picture, and the pane sometimes kept showing the
  previous one. Windows quietly recycles the preview pane when it has been idle, which
  destroys the window we draw into; we kept drawing into the window that no longer existed,
  so the update went nowhere and the stale picture stayed put. We now notice the window has
  gone and rebuild it. (issue #11)

### Added

- **A built-in JPEG 2000 decoder that only decodes what the thumbnail needs.** JPEG 2000
  stores every image as a stack of halved resolutions, but every decoder we could reach
  either could not use that (ImageMagick's reduction flag returns the wrong part of the
  image) or did not exist in usable form - so until now a 76-megapixel scan had to be
  decoded in full just to make a tile. The new decoder reads only the resolution being
  displayed: that scan now thumbnails in 0.25 seconds instead of 4.4, and previews in 1.2
  seconds instead of 5 - and the result is slightly sharper, because a true wavelet
  resolution level keeps detail that shrinking a full decode averages away. Verified
  bit-exact on lossless test images (a correct reversible decode has no rounding excuse),
  and anything the decoder does not support falls back to the old path automatically, so
  no file that rendered before renders worse.

## 1.7.4

### Changed

- **Game textures with mipmaps thumbnail about 17x faster.** A `.dds` file usually already
  contains the picture at every halved size down to 1 pixel, because that is what a game
  engine displays at a distance. We were ignoring all of it and decompressing the full-size
  image to build a small tile. An 8192 x 8192 texture went from 0.87 seconds to 0.05. The
  tile now comes from the texture's own smaller copy, so it looks very slightly softer than
  a full decode would, which is exactly what the game itself shows.

### Fixed

- **"Image info" reported the wrong size for large JPEG 2000 files.** A 9958 x 7686 map scan
  confidently described itself as 4096 x 3161. Nothing could read a `.jp2` header directly, so
  the size was taken from a full decode, and that decode is capped at 4096 pixels for safety.
  The number you were shown was the cap, not the picture. It now reads the real dimensions
  straight out of the file in about a third of a second, without decoding anything.

- **Very large images thumbnailed about twice as fast, and huge scans stopped timing out.**
  A 76-megapixel JPEG 2000 map scan (only 11 MB on disk) took around 9 seconds and, on a busy
  folder, missed the preview pane's time limit entirely, so the pane showed nothing for a file
  that was perfectly readable. The problem was not the file's size: whatever you were going to
  see it at, the decode always rendered a 4096-pixel version first and threw most of it away.
  It now decodes straight to the size being displayed. That scan is about 4.4 seconds for an
  Explorer thumbnail and 5 seconds in the preview pane, and detail is slightly *better*,
  because the picture is no longer resized twice. Most of what remains is the JPEG 2000
  decoder itself. (issue #11)

## 1.7.3

### Fixed

- **AVIF thumbnails had shifted colours, and it was Windows, not ImageMagick.** Files written
  by `avifenc` or `ffmpeg` came out visibly off, while the same files looked right in every
  other viewer. The cause was Windows' own AV1 codec: we asked it first, and it misreads the
  colour information libaom writes by default. Measured against two independent decoders, on
  a saturated test patch out of 255, it was off by 19 on 8-bit files and by 14 on 10-bit ones,
  where even neutral grey drifted from 128 to 139. Other viewers were fine because they do
  not use Windows for this.

  AVIF now goes to ImageMagick except in the one case Windows demonstrably gets right
  (ordinary 8-bit BT.709, which is most web AVIF and stays on the fast path). On the Compact
  install, which has no ImageMagick, nothing changes: you keep the thumbnail you had.
  `scripts/repro-avif-color.ps1` reproduces the whole thing from scratch. (issue #9)

- **Wide-gamut 10-bit AVIF and HEIC lost their colour profile.** A separate fault the above
  uncovered: images decoded through ImageMagick come back at 16 bits per channel, and the
  colour-management step quietly skipped anything wider than 8-bit. An Adobe RGB AVIF was
  landing 79/255 out on a saturated patch. Those images are now colour-managed like every
  other format, which is the same "decoded it right, then threw the profile away" fault that
  was fixed for JPEG XL in 1.7.1.

- **The preview pane could get stuck on the file you were looking at before.** Click through a
  folder of files that need a slower decoder (JPEG 2000 `.jp2` is the usual one) and the pane
  would stop keeping up: it kept showing an earlier file's picture no matter what you selected
  next. The pane keeps one handler alive and re-drives it per file, and when a decode came back
  empty nothing replaced what was already on screen. A single file on its own never hit this,
  which is why it went unnoticed. The pane now always clears first, so a file that fails to
  decode shows an empty pane instead of quietly leaving the wrong picture up. (issue #11)

- **`st2k doctor` claimed thumbnails were disabled when they were not.** Turning off the Windows
  thumbnail *cache* (`NoThumbnailCache` / `DisableThumbnailCache`) was reported as four failures
  saying thumbnails are switched off. Those settings only stop Windows saving thumbnails to disk;
  thumbnails still work, they are just recomputed each time. They are now reported as a note
  rather than a problem.

- **Modern game textures (`.dds`) now thumbnail: BC7, BC6H, BC4 and BC5.** Someone left this
  in an uninstall comment, and they were right. SageThumbs handled the 1998 half of DDS
  (DXT1/DXT3/DXT5) and fell over on everything a game has actually shipped for the last decade.
  BC7, the format most modern colour textures use, only worked on the Full install and only by
  handing the file to ImageMagick. BC6H (HDR), BC4 and BC5 (masks and normal maps) did not work
  anywhere at all: not through SageThumbs, not through Windows' own DDS support, not through
  ImageMagick.

  DDS is now decoded natively, so every block format from BC1 to BC7 renders, HDR textures
  included, on **both** the Full and Compact installs and without shelling out to anything. The
  uncompressed layouts came along with it: 16- and 32-bit float surfaces, 10-bit and 16-bit
  channels, the 565/5551/4444 packings, greyscale and alpha-only textures, and the decade-old
  DirectX 9 files that describe their pixels with bit masks.

- **"Image info" reported nonsense for DDS files.** The header fields were read four bytes off,
  so the mip-level count showed the writing tool's signature as a number (ImageMagick's files
  claimed 1,195,461,449 mip levels) and the compression line was garbage. It now reads the real
  values, tells signed BC4/BC5/BC6H apart from unsigned, and no longer mislabels a BC6H texture
  as BC7.

## 1.7.2

### Fixed

- **Blender `.blend` thumbnails were upside down**
  ([#10](https://github.com/LunarWerxs/SageThumbs-2k/issues/10)). Blender stores its preview
  bottom-up, and we were reading the rows straight through, so every `.blend` came out
  vertically flipped. Now flipped to match, the same way Blender's own thumbnailer does it.

## 1.7.1

**A word about updates.** Ship nothing for a month and people ask whether the project is dead.
Ship twice in one day and people ask you to please, for the love of god, stop. There is no
version of this that everybody likes, so I went with the one where bugs actually get fixed.

More usefully: the automatic update check only ever ran if you had switched on the screenshot
helper, which almost nobody had. For most people it was never checking at all, which isn't
"quiet", it's broken. That's fixed here. If your copy still never mentions a new version,
please tell me through **Send feedback** in the About box. I can't fix what I can't see.

### Added

- **A one-time welcome screen.** SageThumbs has two extras that stay switched off until you
  ask for them, and most people never found out they existed. On first launch it now offers
  both in one small window: **Quick preview** (select a file in Explorer, press Space, see it
  full size) and the **screenshot hotkey** (Ctrl+PrtScn to grab, annotate and copy any part of
  the screen), with an option to use PrtScn on its own instead. Nothing is turned on unless
  you say so, it appears once, and upgrading from an older version never shows it.

### Fixed

- **JPEG XL thumbnails had the wrong colours when the file wasn't plain sRGB**
  ([#9](https://github.com/LunarWerxs/SageThumbs-2k/issues/9)). Every other format already
  honoured the colour profile inside the file; `.jxl` was decoding correctly and then
  ignoring it, so anything from a wide-gamut source (Adobe RGB, Display P3, most modular
  and lossless workflows) came out visibly shifted, while every other viewer showed it
  right. Now colour-managed like the rest. Ordinary sRGB files are unchanged.
- **An update that cannot replace its files now says so.** If Windows is holding a file open,
  the installer can only swap it on the next restart, and until now setup finished looking
  perfectly successful while the PC stayed on the old version. It now checks afterwards and
  tells you plainly, with what to do about it.

- **"Automatically check for updates" now actually runs on every install.** The periodic check
  used to happen only inside the optional screenshot helper, so if you had never switched
  screenshots on, SageThumbs never checked for a new version, even though the setting looked
  switched on. It now checks on its own schedule, and again whenever you open SageThumbs. There
  is still no background service: the check starts, looks once a day at most, tells you if
  there is something newer, and closes. Turning the setting off removes the schedule.
- **A blocked update now tells you it was blocked.** Every failure used to be reported as
  "cancelled at the Windows permission prompt" and then quietly dropped, so if your antivirus
  or a security policy stopped the installer, you saw nothing at all. Now SageThumbs says what
  happened, names Smart App Control when it is switched on, and offers the download page. Only
  an update *you* cancel stays silent.
- **The Windows permission prompt for an update no longer hides behind the download window.**
  The progress window closes before the prompt appears, and the prompt now belongs to the
  SageThumbs window that started the update.

## 1.7.0

### Added

- **Quick preview remembers the size you drag it to.** Previously every file reopened at the
  size the viewer picked for that file, so a window you had just widened snapped back the moment
  you arrowed to the next one. Resize it once and that is now the size, for the next file and for
  every preview after it. Double-click the title bar to forget it and go back to fitting each
  file to its own content. The size is stored independently of display scaling and is clamped to
  fit whatever screen you open it on.
- **Quick preview remembers the volume.** Turning a clip down (or muting it) now carries to the
  next video or audio track, instead of every file starting at full volume again.

## 1.6.0

### Added

- **Windows on Arm is supported, with its own installer.** `SageThumbs2K-Setup-<ver>-arm64.exe`
  runs natively on ARM64 Windows instead of under emulation, and it is a full build: it bundles
  the same ImageMagick engine as the x64 installer, so every format works the same on both.
  The x64 installer is unchanged.
- **EPS files now get thumbnails, without rendering PostScript.** SageThumbs reads the
  safe raster preview already embedded by DOS-EPS, EPSI, or Photoshop, while files with no
  embedded preview still keep their stock icon.
- **Paint.NET `.pdn` files now get thumbnails.** Paint.NET saves a small flattened preview
  inside the file, and SageThumbs reads that, so you see the picture rather than a blank
  icon, with every layer already composited. It works on files from older Paint.NET
  versions as well as current ones, and nothing about the drawing itself is opened.

### Fixed

- **Submenu menu previews now appear reliably.** The preview row is added while the
  right-click menu is built instead of depending on Explorer to send a submenu-open
  notification that it can omit for extension-created child menus.
- **The General settings page no longer crowds the footer.** The checkerboard option now
  lives with the Right-click Menu preview controls it affects, leaving comfortable space
  above Save and Close while the two list-heavy pages keep their intentional scrolling.
- **Screenshot selections are measured against the right screen on mixed-DPI setups.** With
  a scaled display placed to the left of, or above, the main one, the capture overlay sized
  its selection outline, handles and readout for the wrong monitor.
- **Strip metadata now refreshes the file's thumbnail immediately.** Explorer is notified
  after the in-place rewrite instead of continuing to show the cached pre-strip image.
- **HEIC/AVIF edge-case transparency is preserved.** Compact-layout AVIF `mini` containers
  now reach the bundled decoder, and the Full install's isolated thumbnail/preview handlers
  route affected HEIC files around the Windows decoder that flattens their auxiliary alpha.

## 1.5.0

### Added

- **Screenshots of an HDR display are no longer washed out.** Grabbing the screen the
  ordinary way gets what Windows has already flattened to standard range, which on an HDR
  monitor looks pale and grey with the bright parts blown out. SageThumbs now takes the real
  high-range image and maps it down itself, so the shot looks like the screen did. Only kicks
  in on a monitor that is actually in HDR mode; everything else captures exactly as before,
  and if anything goes wrong it quietly falls back rather than failing. On a laptop with an
  external screen, each display is handled on its own, so a mixed setup comes out right.
- **Strip metadata now removes Content Credentials (C2PA).** This is the provenance record
  newer cameras and AI image tools embed, and it is not EXIF or XMP, so ordinary metadata
  cleaners leave it behind. Image info tells you whether a file carries one before you decide.
  A JPEG XT high-range layer uses the same marker and is deliberately left alone.
- **Strip metadata covers more formats:** WebP, SVG and HEIC/AVIF as well as JPEG and PNG. An
  SVG loses the author, machine name and editor history it was exported with, while a
  description inside a shape is kept because that is what a screen reader reads out. HEIC and
  AVIF are cleaned without moving a single byte in the file, so a photo cannot be damaged by
  it; if the layout is anything unfamiliar it declines instead of guessing.
- **Converting keeps your photo's details.** Camera, lens, exposure, date, GPS, keywords and
  captions now survive Convert and Resize instead of being thrown away when the picture is
  re-saved. On by default, switchable in Settings. Shrink for email still removes everything,
  on purpose: that one exists to hand a file to someone else.
- **Convert can write every preset size in one go**, named so you can tell them apart, and can
  **pad to an exact size** with a soft blurred fill instead of leaving you to crop.
- **Comic archives get a proper index.** Combine into CBZ now writes the `ComicInfo.xml` that
  Kavita, Komga and YACReader read, with the page count and each page's size.
- **Combine into PDF can add a page margin**, and can fit pages to A4 or Letter, centred and
  never enlarged.
- **More things preview:** `.woff` web fonts get the same specimen sheet as any other font,
  and `.srt` / `.vtt` subtitle files open as text.
- **Image info shows more of what a file is hiding:** whether it has a high-range gain map,
  the AI-generation tags newer tools write, the names tagged on faces, and for a game texture
  its real compression format and mip count.
- **The colour picker can collect a set.** Ctrl+Space (or Ctrl+click) adds the colour under the
  cursor to a list and keeps picking; Space copies the whole list. Esc still cancels.

### Fixed

- **Esc now cancels a screenshot.** The capture window could end up without keyboard focus, so
  no key reached it at all: the mouse worked, nothing else did, and you were stuck under a
  full-screen overlay. The same fault also meant the colour picker ignored Space and Esc, and
  that right-click windows like Convert could open BEHIND the Explorer window that launched
  them and look like the menu item had done nothing.
- **Camera names no longer appear wrapped in quote marks** in Explorer's columns and details.
- **Files in deeply nested folders get thumbnails again**, including OneDrive-synced ones well
  past the old path-length limit.

### Security

- **Bundled ImageMagick updated to 7.1.2-29**, picking up a set of 2026 fixes for ways its
  security policy could be side-stepped, plus hardening across several older image formats.

## 1.4.1

### Fixed

- **Huge OpenEXR files now get thumbnails.** A 12K VFX render pass, the kind that lands
  hundreds of megabytes on disk with PIZ compression, used to show nothing at all: it was
  simply too big to load, so no thumbnail, no preview pane, no Quick preview. EXRs are now
  read straight off the file and shrunk as they're read, so size stops mattering; a 445 MB
  12288x6480 frame that produced nothing before now thumbnails in a few seconds. Ordinary
  EXRs got much faster too (about 6x on a 12K file) and use a fraction of the memory.
- **Widening the Quick preview window re-wraps the text.** Markdown and other rendered
  documents were laid out in a fixed-width column, so dragging the window wider only added
  empty space at the sides and every line broke in the same place. The text column now
  follows the window, so making it wider really does fit more words per line.

## 1.4.0

### Added

- **Send feedback without leaving the app.** A new **Send feedback** box lets you fire off a
  suggestion, a bug report, or "please support this file format" straight to the developer,
  with no GitHub account needed. Pick what it's about, type your message, and send. Leaving
  an email address is entirely optional; skip it and the message still arrives, you just
  won't get a reply. If you'd rather do it in the open there's a link to the GitHub issue
  tracker right in the box, and if the send fails your text is put on your clipboard so
  nothing is lost. Reach it from the **Send feedback** button in the About box.

- **Read the text off your screen.** The screenshot editor has a new **Copy text (OCR)**
  button, or press **Ctrl+T**: drag out a region, click it, and the words inside land on your
  clipboard *and* open in a small editable window, so you can fix anything the scan misread
  before you paste. Useful for text you can't select any other way: an error dialog, a video
  frame, a screenshared document, a photo of a receipt. It uses the text recognition already
  built into Windows, so it adds nothing to the download. If there are no readable words in
  the region it tells you, instead of quietly copying nothing.
- **A one-key version of it.** Settings ▸ Screenshots ▸ Custom action gained **Copy text on
  screen (OCR)**. Bind it to a hotkey and there is no editor at all: press the key, drag over
  the text, and it is on your clipboard the instant you let go. Don't want to set up a hotkey?
  The same thing is one click away in the tray icon's menu.
- **OCR now reads small on-screen text.** Windows' recognizer quietly gives up on the small type
  most windows actually use, returning nothing at all or running words together. Captures are now
  enlarged before they're read, which is exactly what it needed: text that came back empty or as
  `theothersession's` now transcribes cleanly. Big captures are left alone, since their text is
  already large enough.
- **And a button for it in Quick preview.** Press Space on a picture and the toolbar now has a
  **Copy text (OCR)** button: click it and the words in that file are on your clipboard. It reads
  every format SageThumbs can open, not just the ones Windows understands by itself, so a
  Photoshop file, a camera RAW or a scanned document all work. On a multi-page PDF it reads the
  page you're actually looking at.

### Fixed

- **Chinese, Japanese and Korean text files are readable again.** Quick preview assumed every
  text file was UTF-8, so anything saved in GBK/GB18030, Shift-JIS, Big5 or EUC-KR came up as a
  solid wall of `` replacement characters. Those encodings cover a huge share of real-world
  `.txt`, `.csv` and `.srt` files in those languages. The preview now works out the encoding and
  shows the actual text. Files saved as UTF-16 without a byte-order mark are picked up too.
- **Chinese, Japanese and Thai text now wraps instead of running off the edge.** Those languages
  don't put spaces between words, and Quick preview only knew how to break lines at spaces, so a
  whole paragraph was treated as one enormous word, ran off the right-hand side of the panel and
  got cut off. Text now wraps to the panel like it should, and follows the usual typesetting
  rules: a full stop or closing bracket won't be pushed to the start of a line, and an opening
  bracket won't be left stranded at the end of one.

## 1.3.8

### Fixed

- **HEIC and AVIF thumbnails keep their declared colors.** Files that pair Display-P3
  primaries with a non-sRGB transfer curve are no longer forced through an incompatible
  Display-P3 profile, which could produce a visible color cast. Embedded ICC profiles and
  standard iPhone Display-P3 images continue through the normal color-managed path.

## 1.3.7

### Fixed

- **The right-click preview shows the picture again, without recolouring your menu.** Since 1.3.2
  the preview had been squashed into a strip a few pixels tall on PCs running a third-party menu
  skin such as StartAllBack or ExplorerPatcher. It is drawn full size on those PCs now. Everyone
  else keeps the menu exactly as Windows draws it, because the two kinds of PC need the preview
  drawn two different ways and SageThumbs now picks the right one for yours. There is no setting to
  get wrong: if you have never heard of a menu skin, nothing about your menu changes.

### Added

- **Music files show their album art.** Pressing Space on an MP3, FLAC, M4A, OGG or WMA already
  played the track with a seek bar; the picture area behind it was blank. It now shows the cover
  art stored in the file. Tracks without any keep the plain dark background.

## 1.3.6

- **Explorer stays responsive when menus and metadata appear.** Right-click construction no longer
  decodes the selected file just to prepare a preview; the image is generated only if the
  SageThumbs submenu actually opens. Details-pane metadata probes are header-only, capped at
  250 ms, and limited to two detached workers so slow files or providers cannot accumulate work in
  Explorer or SearchIndexer.
- **Large camera RAW and OS-codec thumbnails use bounded memory.** Shell streams can take an early,
  structurally validated embedded JPEG without reading the sensor-data tail, while WIC scales
  HEIC, AVIF, RAW, and JPEG 2000 frames before copying RGBA pixels into SageThumbs. Ordinary TIFF
  files remain on the normal path, and the configured maximum file size still applies.
- **Pathological Windows metafiles fail quickly.** WMF and EMF decoding now has a dedicated
  three-second/96 MiB ImageMagick budget instead of consuming the broader raster allowance.
- **The optional uninstall note can include reply details.** People who want an answer can leave an
  email or other contact beside their feedback; skipping the survey still never delays removal.
- **Connections sync is safer across devices and accounts.** Nested preference changes preserve
  unrelated remote edits, cache validators cannot cross account boundaries, shutdown flushes
  pending settings, and retry timing honors server throttling.
- **Release packaging is self-contained and icon-verified.** Public artifacts contain the
  installable application without development folders, and release validation checks the visible
  executable and installer branding before publication.
- **Huge archives now return immediately instead of tying up Explorer.** The archive limit
  is enforced before 7z parsing on every shell-stream path, even when Max file size is set
  to Unlimited or a provider omits the filename or length. The reported 909,208,825-byte
  project archive was tested directly and rejected before parsing; network-share timing
  is allowed to vary.
- **Conversions are exact, atomic and memory-bounded.** EXR, HDR, Farbfeld, PAM and PPM
  use native streaming writers with high-bit-depth data preserved where the format allows.
  Every other advertised target uses an explicit bundled writer; unsupported targets fail
  before reading the source, and a failed conversion cannot replace an existing file or
  leave PNG data under the wrong extension.
- **AVIF and JPEG XL export now include their real writer engines.** Earlier Full
  packages advertised these conversions but removed load-bearing ImageMagick
  delegates, which could leave PNG bytes under the requested extension on a clean PC.
  This build retains the required writers, runtimes and legal files, independently
  verifies all 14 Magick-backed output signatures, and rejects incomplete bundles.
- **Screenshot and Quick preview background handling is down to one resident helper.**
  The removed watchdog stays removed, and install/verification restarts the shared daemon
  directly instead of leaving a second command-shell process behind. Full-screen editor
  automation now also exercises Shift-snapped line drawing through the real window.
- **Release builds now fail closed.** Staged contents, exact upgrade-cleanup rules, package
  signatures, source inputs, version, artifact digest, size budget and reviewed release
  notes are all verified before a build can be published.

## 1.3.5

- **Screenshot lines and arrows now snap cleanly while Shift is held.** The live
  preview and saved annotation use the same nearest-45° endpoint, even when Shift is
  pressed or released midway through a drag. The editor also has clearer contextual
  instructions and tool-aware cursors; Esc dismisses the current flyout, text edit,
  drawing or selection before closing the capture, and Ctrl+Shift+Z now redoes.
- **The full-screen screenshot editor can now be tested end to end.** Its window is
  discoverable by Windows UI automation, and an internal synthetic-canvas mode exercises
  the real editor without exposing the desktop or writing to the clipboard, files, or an
  upload service.
- **Large 7z files no longer stall Explorer, especially on network shares.** A 7z over
  the configured maximum file size now keeps the normal 7-Zip icon before SageThumbs
  parses its directory, including when the shell stream does not reveal the filename.
  For archives that are within the limit, 7z reads are buffered instead of turning an
  encoded header into thousands of tiny network round trips; decompression uses one CPU
  thread and no more than 8 MiB of picked-image data in total.
- **Archive contact sheets use bounded memory.** Each image is cropped and reduced as soon
  as it is decoded, rather than retaining as many as four full-resolution images while
  composing a thumbnail.
- **Screenshot hotkeys and Quick preview now use one resident helper process.** The separate
  watchdog process has been removed. The remaining helper still re-registers hotkeys after
  sleep, lock/unlock and remote-desktop changes, and Settings or the next logon restores it
  after a crash or manual termination.

## 1.3.4

- **Quick preview opens common images with less disk I/O.** Static GIF, PNG and WebP files now
  reuse the bytes already read while checking for animation instead of reading the file twice.
  Large project and archive formats also use the same bounded, preview-aware reader as Explorer
  thumbnails, avoiding unnecessary whole-file buffering.
- **Folder navigation follows Explorer's natural filename order.** Moving between nearby files
  now places `image2` before `image10`, and the slim preview scrollbar has a larger invisible
  grab area without taking more room from the document.
- **Release and performance tooling is more trustworthy.** Releases are attached to the exact
  commit validated by CI, the build reports only the installer for the requested version, and
  performance runs isolate every output and report failed decodes plus percentile timings.

## 1.3.3

- **The Quick preview scrollbar now works like a real scrollbar.** In long text, code and
  rendered Markdown previews, you can grab the thumb and drag it to any point in the document.
  Clicking the track above or below the thumb moves by one page, and hover/pressed feedback
  makes the narrow control easier to target. High-resolution mouse wheels and touchpads also
  keep their partial scroll movement instead of occasionally losing it.
- **Updates are verified more defensively before Windows runs them.** The built-in updater now
  accepts installers only from this project's canonical GitHub release path, requires the
  release's SHA-256 digest, verifies the bytes written to disk, and keeps the temporary
  installer protected from replacement through the elevation hand-off.
- **More malformed-input and network-edge cases fail safely.** Bounds and validation were
  tightened across several image/document readers and screenshot-upload URL handling, including
  rejecting ambiguous custom endpoints instead of handing them to Windows' networking stack.
- **Maintenance and build reproducibility.** Compatible locked dependencies were refreshed,
  the largest preview/decoder modules were split into smaller focused files, the real Rust 1.93
  minimum is checked in CI, and third-party GitHub Actions are pinned to immutable revisions.

## 1.3.2

- **Clicking a large archive no longer pegs your CPU and disk.** A big `.7z` could send
  SageThumbs off decompressing a large chunk of the file just to build a thumbnail, because
  the pictures inside a solid archive can sit a long way into the compressed data and
  everything before them has to be unpacked to reach them. SageThumbs now works out up front
  how much it would have to unpack, and if the pictures are buried too deep it just shows the
  normal archive icon instead of grinding away at the file. Archives whose images sit near the
  front still get their preview exactly as before.
- **Archives full of SVG images now get a preview.** A `.zip` or `.7z` containing only `.svg`
  files fell back to the plain archive icon, because the preview builder could not draw
  vector images. It now builds the usual multi-image preview tile from them.
- **The right-click menu is readable again in dark mode.** With the menu preview switched on,
  every entry in the SageThumbs menu turned black against the dark background and was nearly
  impossible to read. The preview picture is now drawn in a way that lets Windows carry on
  theming the rest of the menu, so the text stays light.
- **The Settings buttons now say what they actually do.** The main button reads **Save** in
  every language; in a lot of them it said "OK", which normally means "apply and close", so
  people quite reasonably expected the window to shut and reported it as a bug. The second
  button now reads **Close** instead of "Cancel", since your changes are already saved by that
  point and nothing is being undone.
- **Settings is fully translated again.** 82 pieces of text, including the entire left-hand
  navigation, every page description, several buttons and a batch of tooltips, were still
  appearing in English in all 35 non-English languages. They are now translated everywhere,
  and a new build check keeps any future translation from going missing.
- Thanks to **Bruno** for reporting the dark-mode menu and the confusing Settings buttons.

## 1.3.1

- **"Show quick actions in the menu" works again.** Turning it on is meant to put Convert into,
  Convert…, Resize and Rotate straight on the right-click menu instead of one level deep in the
  submenu, but on many setups it did nothing. If your right-click menu is the classic full one
  (a very common Windows 11 choice), the quick actions were being hidden with nothing to replace
  them. They now show up whenever the option is on.
- **Menu changes take effect right away.** Toggling a menu option in Settings and hitting Save
  now refreshes Explorer's menu, so you see the change on your next right-click instead of having
  to restart Explorer or sign out.
- **Big files and big folders thumbnail much faster.** Large Photoshop (`.psd`/`.psb`), AutoCAD
  (`.dwg`) and 3D-printer G-code files used to be read in full just to pull out the small preview
  buried near the start - so a folder of them filled in slowly, one at a time. SageThumbs now
  reads only the part of the file that holds the preview, which is a tiny fraction of a large
  document. Very large AutoCAD files that previously showed no thumbnail at all now get one.
- **Big EPUB books now show their real cover.** A large e-book could end up showing a random
  picture from inside it instead of the actual cover; it now picks the cover the same reliable
  way small books always did.

- **Copying from a Markdown Quick preview now keeps the document's structure.** Ctrl+C used
  to flatten everything: nested bullets came out flat at one level, headings and quotes lost
  their markers, code blocks ran into the surrounding prose, and every paragraph was jammed
  against the next with no blank line between them. The copied text is now proper Markdown,
  so it pastes with its headings, list nesting, numbering, quotes, fenced code and paragraph
  breaks intact, and still reads correctly if you paste it somewhere plain. Task-list items
  copy as real GFM checkboxes (`- [x]`), and tables still copy tab-separated so they paste
  straight into a spreadsheet as columns.
- **Task-list checkboxes render in the Quick preview.** A Markdown checklist (`- [ ]` /
  `- [x]`) now shows real checkboxes, a filled blue box with a tick for done items and an
  empty box for the rest, instead of the literal `[ ]` / `[x]` text.
- **GIMP `.xcf` files now get a thumbnail, including modern GIMP 2.10 / GIMP 3 files.**
  Older GIMP files worked before, but a file saved by a current GIMP showed nothing at
  all in Explorer. SageThumbs now reads the XCF file itself and flattens its layers into
  a thumbnail, so no separate tool is needed and it works on every install, compact
  included.
- **New: Explorer thumbnails for ZIP/RAR/7z archives.** A plain archive now shows what's
  inside it instead of a generic icon: either a single cover image, or by default a
  contact-sheet collage of up to four images, so a folder of photos zipped up looks
  obviously different from a single photo. Picked the same smart way as comic covers:
  natural filename order, an image named "cover" preferred, junk like `__MACOSX` and
  `Thumbs.db` skipped. Works on huge archives too: the file list comes from the zip's
  central directory and only the picked images are ever read, so a multi-gigabyte zip
  costs a few KB plus a handful of images, in the thumbnail and the preview pane alike.
  Archives with no images, or that are encrypted, keep the normal icon. New toggle in
  **Settings ▸ Ebook/comic**: "Contact-sheet thumbnails for ZIP/RAR/7z" (on by default;
  off gives a single first-image thumbnail, classic CBXShell-style). Comic/ebook archives
  (cbz/cbr/cb7/epub) are unchanged, always showing their one cover.
- **`st2k doctor` can now probe one specific file.** Run `st2k doctor "C:\path\to\that.file"`
  and it checks that exact file end to end: whether the type is one SageThumbs handles,
  whether it's enabled, and crucially whether the file actually *decodes* into a thumbnail.
  This closes a gap where the general self-check could report a clean bill of health while
  one particular file still showed no thumbnail, because the file's format simply can't be
  decoded on this machine. The report now says so, with the reason.
- **Hardened archive reading** against malformed or hostile `.7z`/`.zip` files (updated the
  7-Zip reader and bounded how many entries a crafted archive can make us process).
- **Silent/unattended installs no longer hang on a fresh machine.** A first-time install run
  with `/VERYSILENT` (or in an automated/sandboxed environment) could stall before copying any
  files, because setup tried to stop a background component that only exists after a previous
  install. It now skips that step on a fresh install.

## 1.2.2

- **Works on Windows editions without Media Foundation.** On "N" and "KN" editions of Windows
  (sold in the EU and Korea without media playback components) the shell extension could not
  load at all, so *nothing* worked: no thumbnails for any format, no right-click menu, no
  details in Explorer, and no error to explain it. The video component is now loaded only when
  it is actually needed, so at worst video files lose their thumbnails and everything else
  works.

- **"Repair file associations" and "Rebuild thumbnail cache" now work.** Both buttons closed
  File Explorer and then failed to start it again, leaving an error about a network path that
  could not be found. Nothing was wrong with your network - the commands they ran were being
  quoted incorrectly, so Windows was handed a nonsense path. Both buttons now do what they say,
  and no longer flash a black console window while they work.
- **Setup tells you if registering with Windows fails.** Previously the installer could finish
  and report success while security software had blocked the part that hooks SageThumbs into
  Explorer - leaving you with no thumbnails and no explanation. Setup now checks afterwards and
  says so plainly, with what to do about it.
- **Windows 10: the Convert / Resize / Rotate shortcuts are back on the right-click menu.** They
  were being hidden in favour of the modern Windows 11 menu, which does not exist on Windows 10,
  so they simply disappeared.
- **New: `st2k doctor`.** Run it from the install folder and it checks the whole chain in one go -
  whether Windows has thumbnails switched off, whether SageThumbs is registered with Explorer,
  whether the shell extension can actually load, which of your file types are hooked, and whether
  the decoder itself works. It prints a plain-text report with a verdict and a fix for anything it
  finds, ready to paste into a bug report.
- **"Repair file associations" no longer claims success when it failed.** It used to report
  "repaired" as soon as it managed to *start* the registration step, without checking whether that
  step worked. It now waits for the result, verifies it, and tells you what actually went wrong -
  a declined prompt, a missing file, or something undoing the registration.

## 1.2.1 (2026-07-18)

- **Paint Shop Pro brushes now actually get thumbnails, and tubes get much sharper ones.**
  1.2.0 registered the Paint Shop Pro file family but could only read previews stored as JPEG.
  It turns out `.PspBrush` files never store one that way, so brushes showed no thumbnail at
  all, and `.PspTube` files kept a tiny 80x80 thumbnail next to their real full-size picture,
  so tubes came out blurry. SageThumbs now reads the compressed picture data inside these files
  directly and always picks the largest one available: brushes work, and a tube shows its whole
  contents crisply instead of one fuzzy corner. Also added the legacy `.tub` tube extension.
  (Thanks again to the community member who reported this and supplied test files.)

## 1.2.0 (2026-07-18)

- **The rest of the Paint Shop Pro family now gets thumbnails.** Brushes, picture frames,
  picture tubes, preset shapes, selections and masks (`.PspBrush`, `.PspFrame`, `.PspTube`,
  `.PspShape`, `.PspSelection`, `.PspMask`) use the same file container as `.pspimage`, so
  they thumbnail through the same reader. Note that not every one of these files stores a
  preview picture inside it - where there isn't one, you'll see the normal Windows icon,
  exactly as before. (Thanks to the community member who spotted this.)
- **You can now see the source behind a rendered preview.** Anything the Quick preview renders -
  a Markdown file, a CSV/TSV table, a Jupyter notebook, an HTML page, an SVG - has a new **`{ }`**
  button in the preview's toolbar (or press **Ctrl+U**) that swaps the rendered view for the raw
  file, syntax-highlighted with line numbers and fully selectable. Press it again to go back. The
  mode stays on while the window is open, so **←/→** keeps showing source as you flip through a
  folder; opening a fresh preview always starts rendered. The button only shows up on files that
  actually have both views, so it never appears on a photo or a video.

## 1.1.1 (2026-07-17)

- **Fixed:** on some Windows 11 systems, uninstalling could stop with a **"Resource TSetupForm
  not found"** error and refuse to continue. The uninstaller now completes cleanly.

## 1.1.0 (2026-07-17)

- **You can now select and copy text in the Quick preview.** Drag to select in text, code, log
  **and rendered Markdown** previews - double-click grabs a word, **Shift+arrows** (with
  Home/End/PgUp/PgDn, and Ctrl for whole words) select from the keyboard, and **Ctrl+A** takes
  everything. **Ctrl+C** copies the selection, or the whole document if you haven't selected
  anything. On a Markdown file you get the text as you see it rendered; **Ctrl+Shift+C** copies
  the original Markdown source instead. Ctrl+C works on the other previews too: an image copies
  the picture itself (including the exact PDF page or animation frame you're looking at), and
  the file-info card copies its details. Plain **Home/End** jump to the top/bottom of a document.
- **Fixed:** a Markdown file with an empty heading could close the preview instantly instead of
  showing the file.

## 1.0.1 (2026-07-14)

- **Small vector graphics now convert to a usable size.** Right-click **Convert into PNG** on a
  small SVG icon (or a small `.emf` clip art) used to hand back a tiny image at the file's built-in
  size (as small as 24 pixels). Since these are vectors with no fixed resolution, SageThumbs now
  renders them up to a crisp, usable size, so you get a proper image instead of a postage stamp.
  Larger files are unchanged.

## 1.0.0 (2026-07-14)

**SageThumbs 2K is out of beta.** This release refines the Quick preview and Settings that arrived
in 0.10 and smooths out the rough edges found in real-world use.

- **The preview opens in front, then stays out of your way.** Press Space and the preview jumps to
  the front of your other windows, then behaves like a normal window you can click past or cover.
  The old "always on top" behavior (which could sit over things you were trying to click) is gone.
  You can still pin a preview on top from its toolbar. On by default.
- **Local HTML files render as pages.** Open an `.html` file and the preview shows the real page
  instead of its source. It runs locked down (WebView2 with scripts off and no network), so a local
  page only ever shows its own content. On by default.
- **Smoother text, code, and log scrolling.** Scrolling a large text file no longer flickers or
  stutters, even very big files stay smooth, line numbers are shown, and there is now a scroll
  position indicator on the side.
- **Smoother Markdown scrolling.** Large Markdown documents scroll cleanly instead of lagging.
- **A crisper Settings window.** The toggles, icons, and buttons are now smoothly anti-aliased
  instead of looking jagged.

## 0.10.0 (2026-07-13)

- **New: press Space to preview a file, QuickLook-style.** Select a file in Explorer (or on the
  Desktop) and tap **Space** for an instant full-size popup, then Space or Esc to close. It shows
  any format SageThumbs can decode, and adds a lot a still image can't: **videos and audio play**
  (with a seek bar, play/pause and volume), **animated GIFs/WebP animate**, **code is
  syntax-highlighted with line numbers** (editor-style, across the common languages - JSON keys,
  strings and numbers each get their own colour), **Markdown renders GitHub-style** (with clickable
  links), **multi-page PDFs
  page** with the arrow keys or on-screen buttons, **fonts show a specimen** (name + pangram + glyph
  sheet), and **archives (zip/7z/rar) list their contents** without extracting. Use **←/→** (or
  PgUp/PgDn) to flip through the folder without closing, and **F11** for full-screen. You can zoom
  and pan images with the wheel, and the popup never steals focus from Explorer. It's **off by
  default**: turn it on under **Settings ▸ Quick preview**, where you can also tweak hold-to-peek,
  close-on-focus-loss, pin-on-top, and which content types are previewed.
- **New: Markdown preview has a collapsible outline sidebar.** When a Markdown file has headings,
  the Space-bar preview shows a **Contents** panel on the left that lists them as a clickable,
  indented outline. Click any heading to jump straight to that section, and the section you are
  currently reading stays highlighted as you scroll. Toggle the panel with the outline button in the
  toolbar; your choice is remembered for next time.
- **New: CSV and TSV files preview as real tables.** Press Space on a `.csv`/`.tsv` and you get a
  gridded, shaded table view (quoted fields, embedded commas and multi-line cells handled;
  semicolon-separated exports detected automatically) instead of raw text. Very large files show
  the first thousand rows with a note.
- **New: Jupyter notebooks preview rendered.** `.ipynb` files show their markdown cells rendered,
  code cells syntax-highlighted with line numbers, and text outputs (including cleaned-up error
  tracebacks) - no Jupyter install needed.
- **New (optional): download web images in Markdown previews.** Off by default: badges and other
  web-hosted images show as labeled chips. Turn it on (Settings ▸ Quick preview) and they download
  (HTTPS only, size-capped, in the background) and display like GitHub shows them.
- **Outline sidebar polish.** Clicking an entry near the end of a document now visibly selects it
  even when the page is already scrolled to the bottom, and the panel slides open and closed
  instead of snapping.
- **Bare links are clickable now.** A URL typed straight into Markdown (like `https://example.com`
  or `www.example.com`) turns into a clickable link, the way GitHub does it. You no longer have to
  wrap it in `[text](url)`. URLs inside code stay plain.
- **Markdown now renders the way GitHub shows it.** README-style pages come out looking right:
  the HTML "hero" section at the top (centered banner, title, tagline, badge row) renders instead
  of disappearing, **pictures stored next to the file display inline** (sized like GitHub sizes
  them, clickable when they're links), and **tables get real grid lines, shaded alternating rows,
  and sensible column widths**. Common inline HTML (`<b>`, `<i>`, `<br>`, links, lists, tables,
  `<details>` blocks) renders too, and the text column is capped and centered like a GitHub page
  instead of stretching edge-to-edge. Web-hosted images (like status badges) show as small labeled
  chips rather than being downloaded - previewing a file never touches the network.
- **Optional: render local HTML files.** With the new **"Render local HTML files"** toggle
  (Settings ▸ Quick preview, off by default), pressing Space on a `.html` file shows the rendered
  page in an embedded, **locked-down** viewer - JavaScript is disabled and every network request is
  blocked, so a page cannot phone home or load remote trackers. A companion **"Live-load .url
  shortcuts"** toggle (also off by default) can open a `.url`'s real web page in a throwaway session;
  left off, a `.url` just shows its target address as text.
- **Animated (CSS-driven) SVGs no longer show up blank.** SVGs that hide their artwork at rest and
  reveal it through a CSS animation now render their visible state everywhere (thumbnails, the
  preview pane, and the new Space-bar preview) instead of an empty frame.
- **SVG images now show a picture in the right-click preview.** Right-clicking an `.svg`
  (or `.svgz`) used to show just the filename and size at the top of the SageThumbs menu, even
  though the file thumbnailed normally everywhere else. It now shows the actual image there too.
  (Video, PDF, and a handful of rare specialist formats still show name + size only in that menu
  tile (rendering them in-place could momentarily freeze Explorer) but they still thumbnail
  normally in the folder view and preview pane.)

## 0.9.0 (2026-07-09)

- **The preview pane now handles big files like thumbnails do.** Explorer's reading/preview pane
  used to read a file whole before showing it, so a multi-gigabyte video, a long audiobook, or an
  oversized comic/`.blend`/Photoshop file would either bog down the preview host or just show a
  blank pane, even though its thumbnail worked fine. The pane now uses the exact same shortcuts the
  thumbnail does: it grabs a single video frame, seeks straight to embedded album art, or pulls the
  cover out of a huge archive without ever loading the whole file, and it respects your size limit.
- **DICOM `.dcm` medical scans finally thumbnail.** They were listed as supported but never
  actually rendered: the file's TIFF-like header fooled the decoder into treating it as a broken
  TIFF. CT/MR slices now show real, legible anatomy (the low-contrast medical data is auto-stretched
  so it isn't just a flat gray square).
- **Apple `.icns` icons actually work now.** They were listed as supported but no decoder ever
  handled them; the embedded PNG (or JPEG-2000) icon is now extracted directly.
- **If you sync settings, your name shows up instead of a random email.** The "Synced as" row
  used to show an opaque per-app privacy-relay address; it now shows your actual display name
  (falling back to the relay address only if no name is available).
- **Release-readiness polish.** Licensing docs, the supported-format counts quoted across the
  README/docs, and some duplicated internal code were reconciled so everything lines up; file
  renames during Convert/Resize are now atomic (no half-written output if something goes wrong
  mid-write).
- **Screenshot capture fixes.** A stray 1px window border and the invisible DWM resize border no
  longer sneak into `--shot`-style captures used for the app's own documentation screenshots.
- Updated the DjVu decoder dependency to pick up upstream fixes and trim unused transitive deps
  (no user-facing behavior change).

## 0.8.0

- **Compressed `.blend` files now show thumbnails at all.** Files saved with Blender's
  "Compress" option (gzip or zstd) previously never got a preview; now they do, at any size.
- **Big Blender files finally show thumbnails.** `.blend` scenes over the size limit (100 MB by
  default) were silently skipped by Explorer even though the thumbnail sits in the first few
  kilobytes of the file; now we read just that small head slice, so a 2 GB scene thumbnails
  instantly. Same fix for huge Photoshop `.psd`/`.psb` files. (Thanks to GitHub issue #1.)
- **Big Clip Studio Paint canvases show thumbnails too.** A multi-layer `.clip` over the size
  limit was skipped even though its preview lives in a small database at the end of the file;
  we now jump straight to that database and read only it, so a 2 GB manga page thumbnails
  instantly. Works in the preview pane and the `st2k` CLI as well.
- **See-through EXR render passes show their content.** An OpenEXR whose alpha channel is
  entirely empty (emission/AOV/environment passes) used to show a blank default icon; it now
  renders its actual colors. Note: **DWAA/DWAB-compressed EXR needs the standard install** (the
  bundled ImageMagick decodes it); uncompressed/ZIP/PIZ/B44 EXR work everywhere. (GitHub issue #2.)
- **Old 32-bit TGA files no longer come out invisible.** Files whose header declares "no alpha"
  but still carry a (meaningless, all-zero) 4th channel used to decode fully transparent, in
  thumbnails, Convert, and the AI `view` tool. They now render opaque, as every image viewer does.
- **Huge Krita/OpenRaster/3MF/FreeCAD files show the right preview.** Oversized project files
  used to get an arbitrary internal layer image as their thumbnail (often blank); they now get
  the real composite preview, same as small ones.
- **Amiga IFF/ILBM images with a transparency mask render it correctly** (masked areas used to
  come out opaque).
- **`.jbig` removed from the supported-formats list.** It never actually decoded (no shipped
  decoder can read it); the entry only cost a doomed 20-second attempt per file.
- **Sync your settings across your PCs (new, and completely optional).** Settings has a new
  **Data & Backup** section with a **"Sync settings…"** button: sign in with a Connections account
  (it opens your real browser, SageThumbs never sees your password) and your portable preferences
  follow you to every machine you sign into: thumbnail limits and quality, the right-click menu layout
  and toggles, hotkeys, language, and ebook/comic options. It's **off by default** (no network happens
  unless you turn it on), only an allowlist of portable settings syncs (**never your files, folder
  paths, or passwords, and never your images**), and your settings always stay on your PC too, so
  everything keeps working fully offline or signed out. Disconnect anytime and the cloud copy is
  removed. As always, the thumbnail shell extension itself never touches the network; all sign-in and
  sync code lives in the Settings app only.
- **Your sign-in is stored securely.** The token that keeps you signed in is encrypted on your machine
  with Windows' own DPAPI (only your account, on that PC, can read it) and is never part of the synced
  data; the cloud copy is a plain "settings locker," no secrets.

> Upgrading from 0.7.1? This release also rolls in everything under **0.7.2** below (hotkey resilience,
> capture/upload feedback, and the CLI fixes).

## 0.7.2

- **Hotkeys now survive sleep, lock, and updates.** Windows silently un-registers global hotkeys
  after sleep/resume, locking your PC, or a remote-desktop reconnect; the background helper now
  re-registers them the moment those happen (plus a once-a-minute safety net), so your screenshot
  hotkey keeps working instead of quietly dying until you reopened the app. App updates and
  reinstalls also restart the helper automatically. Previously an update silently killed your
  hotkeys until the next sign-in.
- **The tray icon survives Explorer restarts.** When Windows Explorer crashes or restarts it wipes
  all tray icons; the helper now puts its icon back automatically (and retries at sign-in if the
  taskbar isn't ready yet).
- **Copying a screenshot is more reliable.** If another app was momentarily holding the clipboard
  (clipboard managers and Office do this constantly), your capture's copy could silently do
  nothing; it now retries briefly instead of giving up.
- **One capture at a time.** Pressing the screenshot hotkey twice no longer stacks a second frozen
  overlay on top of the first.
- **Bind OCR to your custom hotkey.** "Copy text (OCR)" joins the Quick Action list; press your
  hotkey over the selected image(s) and the recognized text lands straight on the clipboard.
- **"Sort into folders ▸ By image size" is much faster on big selections.** Reading each file's
  dimensions now runs in parallel like the other batch actions (exotic RAW/HEIC files used to be
  probed one at a time).
- **CLI: `st2k batch` now fails properly.** It exits with an error when every file failed
  (partial runs report how many failed) so scripts and automations can detect it. Also `st2k pdf`
  now honors your configured JPEG quality instead of a fixed 85, and OCR errors say what actually
  went wrong.
- **The quick-save hotkey now shows it worked.** A split-second screen flash confirms the capture
  (like Win+Shift+S); if the copy or the PNG save failed, a small notification tells you exactly
  what went wrong instead of total silence.
- **See when a hotkey is taken by another app.** If some other program owns your chosen chord,
  Settings ▸ Screenshots now says so ("hotkey in use by another app") instead of claiming
  everything is running, and it clears itself within a minute of the other app letting go.
- **Uploads show an "Uploading…" indicator.** Both the screenshot Upload button and right-click ▸
  Upload now show a small progress pill while the transfer runs, no more staring at nothing
  wondering if the click registered.

## 0.7.1

- **See a screenshot's exact size while you drag it.** When you drag out a region to capture, a small
  `width × height` readout now sits at the corner of the selection, so you can size things precisely (in
  real pixels).
- **The screenshot / action hotkeys stay working.** The small background helper that powers the global
  hotkeys now restarts itself automatically if it ever stops, and just opening Settings brings it back if
  it was down, so your hotkey won't quietly stop firing. Its live status shows under Settings ▸ Advanced
  ▸ "Hotkey service".
- **New: right-click ▸ Upload (copy link).** Right-click an image (or several) and upload straight to a
  free, no-account host (catbox.moe by default), with the link(s) copied to your clipboard; your original
  files are left untouched. The resulting links open in a small window you can select and copy from.
- **About box opens centered, with the proper GitHub icon.** The About window now appears centered over
  Settings instead of stuck in the top-left corner, and its GitHub badge shows the real GitHub logo.
- **"Hide tray icon" moved to Advanced.** That toggle now lives under Settings ▸ Advanced ▸ "Hotkey
  service", next to the Restart button.

## 0.7.0

- **Redesigned Settings window.** The old single long scroll is gone. Settings now opens with a
  Windows 11-style **category rail** down the left (General · File types · Ebook/comic · Right-click
  menu · Screenshots · Quick action · Advanced) and a clean content page on the right, with on/off
  **toggle switches**, category icons, and a titled header per page. Everyday options sit up front;
  diagnostics, updates and backup tuck under **Advanced**. Same settings, far less clutter. (The new
  labels are translatable; languages without the new strings yet fall back to English.)
- **Assign your own hotkey to a tool.** Pick an action (color picker, take a screenshot, Convert…,
  rotate, move-to-folder, strip metadata, or open Settings) and a keyboard shortcut, and that shortcut
  now works anywhere. The file actions run on whatever you've got selected in Explorer, or pop a file
  picker if nothing's selected. It reuses the existing screenshot helper, so there's no extra
  background program.
- **Cleaner right-click menu on music files.** Right-clicking an audio file (MP3, FLAC, …) no longer
  shows image-only actions like Resize, Rotate, or Set as wallpaper; just the ones that make sense
  (move to folder, rename by tag, sort by tag).
- **AVIF / JPEG XL quality slider.** The Convert… dialog now lets you set the quality for AVIF and
  JPEG XL output (it only had this for JPEG and WebP before).
- **+1 format: DSD audio (`.dsf`).** Album-art thumbnails for DSD audio files, now **316** supported
  file types.
- **Fixed: Photoshop files with a transparent background now preview correctly.** If you removed the
  background in Photoshop and saved, the thumbnail used to show a solid **white** background, because it
  came from Photoshop's built-in preview image, which can't store transparency. SageThumbs now renders the
  actual layered image (keeping the transparency) for transparent PSD/PSB files. This was never a
  refresh/cache problem: the thumbnail was always current, just flattened. (Needs the full install; the
  compact, ImageMagick-free build still falls back to the white preview.)
- **Fixed: dimensions now appear in the Explorer details pane.** The 0.6.0 update added image
  dimensions, camera info, and audio tags for the formats Windows can't read, but they only showed up
  in a file's Properties window and its hover tooltip, *not* in the details pane along the bottom (or
  side) of the Explorer window, where a PSD, camera RAW, EPUB, etc. still listed only its date and size.
  They now show there too.
- **A lot more file info in Explorer.** While fixing the above we found the handler was reading several
  useful facts and then throwing them away. Now, for the 300+ formats Windows can't read, Explorer's
  Details pane / Properties / columns also show: **date taken, GPS location, color depth and DPI** for
  photos and camera RAW; and **length (duration), bitrate, genre and year** for audio (OGG, Opus, AIFF,
  Musepack, …). Camera RAW even gets its GPS location where Windows itself shows nothing.
- **Those columns are now offered, not hidden.** You can right-click a column header (or "Choose
  columns…") in a folder of PSDs/RAWs/etc. and actually pick Dimensions, Date taken, Length, Artist, …
  as a **sortable/groupable column**; previously the data existed but Explorer never offered it for
  those file types. The files are also classified for `kind:` search (e.g. Krita/OpenRaster as pictures).
- **Fixed: "Show menu on all file types" now works on Windows 11's default menu.** The setting that adds
  a small file-utility menu (move to folder / sort / rename / pick color) to *unsupported* files only
  took effect on the old "Show more options" menu; on the modern Win11 right-click menu it did nothing.
  Now it works there too.
- **Fixed: more video formats get thumbnails.** **`.ts` / `.m2ts` / `.mts` (MPEG transport streams) and
  `.ogv` (Ogg video)** were registered but always showed a blank icon; they were being routed to the
  wrong decoder. They now use the OS video path like every other video. (`.flv` and raw `.mpg`/`.m2v`
  are routed correctly too, but only show a frame if Windows actually has that codec installed.)
- **Fixed: "Keep original file date" now applies to the Convert dialog.** The toggle worked for the
  quick one-click converts but was skipped by the **Convert…** dialog, so its output always got the
  current date. It's honored everywhere now.
- **Fixed: searching by the info we add now works.** The dimensions/camera/audio details showed in the
  Details pane but were stored in a form Windows Search wouldn't index, so "find by artist/camera/date"
  never matched our files. They're now stored in the canonical form the index and column-grouping expect.
- **Fixed: "Files to folder" tells you when it can't.** If creating the folder or moving the files failed
  (read-only, locked, different drive), the dialog used to just close as if it worked. It now shows a
  message and stays open so you can retry. The global-hotkey actions and the screenshot save now report
  failures too, instead of silently doing nothing.
- **+ audio length, bitrate, genre and year for WMA**, and a cleaner uninstall that no longer leaves
  stray registry entries behind (including from very old versions). Under the hood: a security hardening
  pass on the one-click updater (re-verifies the installer on disk right before it runs).

## 0.6.3

- **One-click updates.** When a new version is available, **Settings ▸ Check for updates** can now
  download and install it for you: a progress bar shows the download, Windows asks once for
  permission, and the update installs in the background and confirms when it's done. No more hunting
  down the installer by hand. (You can still grab it from the releases page if you prefer.)

## 0.6.2

A bug-fix release centered on a serious file-dialog problem, plus a sweep for anything like it.

- **Fixed: file dialogs could hang for up to ~2 minutes.** Opening a file picker (for example,
  attaching or uploading a file in your browser) could freeze for a long time as the dialog closed,
  and the preview pane could come up blank/white. The image preview now runs on its own
  message-pumping thread, so closing the dialog is instant and the preview paints reliably. *(This
  was the big one.)*
- **Preview pane now follows your theme.** The preview's background matches Windows dark/light mode
  instead of always being white, even when the host dialog hands the preview the wrong color.
- **Fixed: an unusual or corrupt file can no longer stall the shell.** A hardening pass put a strict
  time limit (and crash-safety guard) on *every* in-process decode path: PDF thumbnails, the
  right-click menu preview, OCR, the Details/property handler, and the SVG / video / camera-RAW
  helpers, so no single file can freeze Explorer, a file dialog, or the preview host. Earlier builds
  could stall on a malformed PDF or a very large image.
- **Fixed: a rare crash when closing a file dialog.** Background decode helpers now keep the
  extension loaded until they finish, so the shell can't unload it out from under a running decode.

## 0.6.1

- **Crisp thumbnails at large/Hi-DPI icon sizes.** Raised the maximum generated thumbnail
  edge from 256 px to 1024 px. On 4K displays and the larger ("jumbo") icon views, Explorer
  asks for thumbnails bigger than 256 px; we used to hand back an undersized 256 px image,
  which looked soft *and* couldn't be cached durably, so Explorer re-generated it on every
  refresh, re-decoding a frame from each (potentially multi-GB) video each time. Now we honor
  the requested size up to 1024 px, so big thumbnails are sharp and stay cached. Smaller views
  are unchanged.
- **Audio waveform thumbnails.** WAV, AIFF and AIFF-C files with no embedded cover art now
  show a drawn waveform instead of a blank icon, a quick visual of the sound. Files that do
  have album art still show the artwork; compressed formats (MP3/FLAC/…) are unchanged.
- **+1 format: AIFF-C (`.aifc`).** The audio handler now also covers AIFF-C, bringing the
  total to **315** supported file types.

## 0.6.0

- **Details in Explorer for 300+ formats Windows can't read.** A new property handler
  surfaces image dimensions, EXIF camera info, and audio tags in Explorer's Details pane,
  hover tooltips, and sortable/groupable columns, for the formats Windows has no idea how
  to read on its own. Read-only and crash-isolated behind a panic boundary, like the
  thumbnail provider.
- **Proper color management.** Embedded ICC profiles and wide-gamut images (Display P3 /
  Adobe RGB) now render in correct sRGB instead of looking over-saturated. AVIF/HEIC read
  their profile from the ISOBMFF `colr` box (including the CICP nclx Display-P3 signal that
  iPhone HEIC uses), and CMYK JPEGs are color-managed through their embedded CMYK profile.
  Pure Rust; no C dependencies.
- **Autodesk Fusion 360 (.f3d)** thumbnails, read from the zstd-compressed preview inside
  the file's ZIP container, bringing the total to **314**.
- **Repair file associations** button (Settings → Diagnostics): re-registers SageThumbs for
  all your enabled formats when another app has taken over the thumbnails, then clears the
  thumbnail cache.
- **MCP `view` and `compress` tools.** The AI/agent server gained a `view` tool that decodes
  any of the 314 formats to a PNG image block so an agent can actually *see* the file, plus a
  `compress` tool.
- **Unused ImageMagick text shaping was removed.** The bundled
  glib/harfbuzz/freetype/fribidi/raqm stack is stubbed because SageThumbs does not invoke
  ImageMagick's text, caption or font-rendering surfaces.
- **Hardening.** Fuzzing and Miri over the untrusted-input parsers, COM round-trip tests for
  the preview and property handlers, dead-code cleanup, and the test corpus extended to cover
  all 314 formats.

## 0.5.0

- **Video thumbnails, done properly.** Explorer now reliably shows a thumbnail for your
  videos, and it's a *representative* frame from about a third of the way in, not the black
  intro, fade-in, or studio logo you'd get from the opening frame. Covers **MP4, MOV, M4V,
  MKV, WebM, AVI, and WMV**.
- **Fast even on huge 4K files.** For MP4 and MKV we read the video's own index and pull just
  the single frame we need (a few megabytes) instead of scanning the file, so a folder of
  multi-gigabyte movies on a slow drive thumbnails quickly, and can no longer peg a CPU core
  or leave blank tiles that never resolve.
- Formats Windows itself has no codec for (MPEG-1/2 **.mpg/.mpeg**, Flash **.flv**) keep the
  normal file icon; nothing can produce a thumbnail for them without an installed codec.

## 0.4.9

- **Correct colors for wide-gamut photos.** Thumbnails of Display-P3 / Adobe RGB images
  (most modern phone and camera photos) are now color-managed to sRGB, so they match what
  you see in Photos or a browser instead of looking over-saturated.
- **Crisp pixel art & icons.** Tiny images (sprites, 16–64 px icons) now scale up sharp
  instead of being blurred into a smudge.
- **Compress to a target file size.** The `st2k` command-line tool gained
  `compress <file> --max-size 1MB` (or `500KB`, etc.); it finds the best quality that
  fits under your size limit.
- **No more stuck blank thumbnails.** If a file decodes to nothing, Explorer now shows the
  normal file icon instead of caching an empty tile you couldn't clear.
- **Apple Live Photos (.livp)** now show their still image, bringing the total to **313**.

## 0.4.8

- **Thumbnails now work on a clean Windows install.** The shell extension no longer
  depends on the Visual C++ runtime, so it registers and shows thumbnails even on a fresh
  machine that's missing the VC++ redistributable; previously that produced no thumbnails
  and a cryptic "failed to register" error during install.
- **More EPUB covers show up.** Books that reference their cover through a wrapper page
  instead of the image directly (e.g. Standard Ebooks and many older EPUBs) now display
  the real cover rather than a blank icon.
- **Very large comic archives thumbnail again.** A CBZ or CB7 over 256 MB now shows its
  cover, read straight from the archive without loading the whole file into memory, instead
  of falling back to a generic icon.
- **Two more formats**: GeoGebra worksheets (**.ggb**) and **.phz** comic archives,
  bringing the total to **312**. A JPEG-2000 page inside a comic archive can now serve as
  the cover too (on the full install).
- **DjVu hardening verified**: the specific scanned documents that crashed the previous
  generation of this kind of extension render cleanly here.

## 0.4.7

- **Fixed preview-pane hangs.** Selecting an image in a file dialog or the Explorer reading/
  preview pane could freeze and sometimes need the preview host killed (or a reboot). Previews
  now decode off the host's UI thread, and an internal concurrency lock that could leak when a
  host was force-killed is now self-healing, so the hang can no longer build up over time.
- **Right-clicking an exotic file no longer freezes Explorer.** The classic right-click menu's
  preview now uses only the fast built-in decoders, never a slow external one on the shell
  thread.
- **Video previews and thumbnails are time-bounded**, so a stalling codec can't hang the
  preview or thumbnail.
- **Right-click actions run in the background.** Convert, Resize, Rotate, Strip metadata, and
  the rest no longer freeze the Explorer window while they work, even across many files.
- **Automatic update check.** Opening **Settings** now does a quiet, once-a-day background
  check for a newer version and flags the "Check for updates" button when one is available;
  no nagging pop-ups, and never more than once a day.

## 0.4.6

- **Video thumbnails**: Explorer now shows a representative frame for video files
  (Matroska **.mkv**, **.webm**, **.mp4**, **.mov**, **.avi**, and more) using the OS's own
  codecs, so it bundles **zero** extra bytes and streams the file instead of loading it.
- **Settings import / export**: back up your whole configuration, or move it to another PC,
  as a single human-readable JSON file (Settings → Diagnostics).
- **Check for updates**: a button that asks GitHub whether a newer release is out and points
  you to the download (Settings → Diagnostics).
- **Rebuild thumbnail cache**: clears Windows' stale thumbnail cache and restarts Explorer,
  so a format/size change shows up immediately (Settings → Diagnostics).
- **More reliable camera-RAW thumbnails**: RAW files now fall back to their embedded preview
  even when it's small, so they thumbnail on a clean Windows install with no extra codecs.

## 0.4.5

- **Screenshot capture tool:** explicit **Ctrl+C** (copy) / **Ctrl+S** (save) keys, plus an
  optional fixed save folder for Ctrl+S (otherwise it prompts each time).

## 0.4.4

- **Fully customizable right-click menu**: drag to reorder entries *and* their dividers
  (WYSIWYG), and show/hide any item; the menu mirrors your layout exactly.
- "Tools" submenu flattened to individually toggleable top-level verbs; a **"Show menu on all
  file types"** option (a condensed file-utility menu on unsupported files).
- **Image info** is now a verbose, copyable dialog: every EXIF tag plus a GPS map link.
- Settings window is **vertically resizable**, with flicker-free scrolling.
- **Diagnostics** section: a user-sendable log with crash capture (Settings → Diagnostics).

## Earlier (0.4.x)

- **288 file formats**: camera RAW, Photoshop (PSD/PSB), HEIC/AVIF, JPEG XR, JPEG XL,
  MS Office, DjVu, ebooks & comics, 3D-print files, and the obscure long tail.
- **Right-click toolkit**: convert, resize, lossless rotate/flip, combine-to-PDF / -CBZ,
  shrink-for-email, OCR, a system-wide eyedropper, strip metadata, copy, set-as-folder-icon,
  set-as-wallpaper, and folder utilities. Multi-file jobs run in parallel across every core.
- **Native Windows 11 UI** with system-following **dark mode** and **36 languages**.
- A searchable **per-format on/off** list, and tunable thumbnail size + JPEG/PNG quality.
- Built-in **screenshot capture** tool with a configurable global hotkey.
- **Crash-isolated**: a corrupt or malicious file can't take down File Explorer (runs
  out-of-process, panic-guarded, with a sandboxed decoder).
