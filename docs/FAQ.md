# SageThumbs 2K FAQ

Short answers to the things people actually ask. If your problem is thumbnails not appearing,
start with **`st2k doctor`** (below): it checks the whole chain and prints a fix for each thing
it finds, which is faster than guessing.

- [Thumbnails](#thumbnails)
- [Press Space to preview](#press-space-to-preview)
- [Right-click menu](#right-click-menu)
- [Antivirus and SmartScreen](#antivirus-and-smartscreen)
- [Portable version](#portable-version)
- [Settings, updates, uninstalling](#settings-updates-uninstalling)
- [Formats](#formats)
- [Licensing](#licensing)

---

## Thumbnails

<details>
<summary><b>Thumbnails are not showing up at all</b></summary>

Run this in a terminal and read what it says:

```
st2k doctor
```

It is read-only, it walks the entire chain (Windows' own thumbnail switches, our four
registrations, whether the DLL actually loads, whether another program has taken over the file
types, your settings, and a live decode test), and it prints a specific fix under anything that
is wrong. When asking for help, use its zip export, which bundles its findings with the relevant tail of
the error log, instead of pasting output by hand. You will get a much faster answer.

The three most common causes it finds:

1. **Windows is set to "Always show icons, never thumbnails".** File Explorer, View, Options,
   View tab, untick it. A group policy can also force this on managed machines.
2. **Another program took the file types.** Photo tools often claim `.jpg`, `.png` and RAW on
   install. Fix it in Settings, Advanced, **Repair file associations**, which re-registers every
   format you have enabled and then clears the thumbnail cache.
3. **Your antivirus quarantined the DLL during setup.** See
   [Antivirus and SmartScreen](#antivirus-and-smartscreen).

</details>

<details>
<summary><b>Some thumbnails appear, others stay as blank icons</b></summary>

Windows caches thumbnails aggressively, and a file it failed on once stays failed. Settings,
Advanced, **Rebuild thumbnail cache** clears Windows' cache and restarts Explorer.

If it is a whole format rather than scattered files, check that format is ticked in Settings,
File types.

If it is one particular video, run `st2k doctor <that file>` and read the **Video codec** line.
Video frames decode through the codecs Windows ships, and two things stop that: a codec Windows
does not have (HEVC and AV1 are Store add-ons, and the doctor names the one to install), or an
H.264 file in a profile the Windows decoder does not implement at all (4:4:4, 4:2:2 or 10-bit
colour, which some encoders write when asked for `yuv444p` or similar). The second kind is
skipped on purpose and instantly, because the Windows 10 decoder hangs on such files rather
than declining them. Re-encode as ordinary 8-bit 4:2:0 H.264 (`ffmpeg -c:v libx264 -pix_fmt
yuv420p`), or attach cover art, which is shown whenever no frame can be decoded.

</details>

<details>
<summary><b>A video I rotated plays upright everywhere but shows sideways in the thumbnail</b></summary>

That's fixed. Some editors write a rotation flag instead of re-encoding the pixels (for example
`ffmpeg -display_rotation 90 -i in.mp4 -c copy out.mp4`), which is instant and keeps full
quality. Windows and practically every player honour that flag; older SageThumbs versions did
not, so the thumbnail matched the original orientation instead of the rotated one. It now reads
the flag and turns the picture to match, for both the MP4 family (`.mp4`, `.mov`, `.m4v`,
`.3gp`) and Matroska (`.mkv`, `.webm`). If you still see a sideways thumbnail, run `st2k doctor`
to check your version.

</details>

<details>
<summary><b>Photoshop thumbnails look soft or undersized in the large preview pane</b></summary>

A `.psd` or `.psb` carries a small built-in thumbnail, usually about 160 pixels regardless of the
artwork's real size. Explorer's large preview pane and Quick preview ask for something closer to
2048 pixels, and older versions answered that request with the same small built-in thumbnail, so
the picture never got sharper no matter how long you waited. It now renders the full-resolution
artwork whenever the built-in thumbnail is too small for what was asked. Ordinary icon views are
unchanged and just as fast, since the built-in thumbnail really is big enough there. The Space-bar
Quick preview is fixed the same way, including for PSD/PSB files over about 256 MB, which
previously stayed stuck on the small thumbnail with nothing in the log to explain why.

</details>

<details>
<summary><b>Thumbnails work in a folder, but the file is blank in OneDrive</b></summary>

If the file is online-only, there are no bytes on the disk to read. Windows will not download a
file just to draw a thumbnail, and neither will we. Mark the folder **Always keep on this
device** and the thumbnails appear.

</details>

<details>
<summary><b>A file shows a thumbnail in Explorer but a plain caption in the right-click menu</b></summary>

That is deliberate, not a bug. The little preview tile inside the right-click menu is drawn
**inside `explorer.exe` itself**, so it only runs the cheap, safe decoders. Video frames, PDF
pages and anything that needs the bundled ImageMagick are skipped there and show the file name
and size instead. Those same files still thumbnail normally in the folder and in the preview
pane, which run in their own isolated process where a slow or hostile file cannot hurt Explorer.

---

</details>

## Press Space to preview

<details>
<summary><b>Nothing happens when I press Space</b></summary>

It is **off by default**. Turn it on in Settings, Quick preview. It runs a small background
helper, the same one the hotkeys use.

</details>

<details>
<summary><b>It works in Explorer but not in Everything</b></summary>

**Click a result first.** While your cursor is still in Everything's search box, Space types a
space, which is what it should do. The preview is only ever offered to the result list.

Both **Everything 1.4 and 1.5** work, installed or portable, under any instance name. If Space
still does nothing after you have clicked a result, check the next question. An Everything
running as administrator is by far the most common cause, and it looks exactly like this.

</details>

<details>
<summary><b>It does not work when Everything runs as administrator</b></summary>

**Space cannot work there, but a hotkey can.** Windows deliberately stops a normal program from
seeing keys typed into a program running as administrator. Our background helper is a normal
program, so when an administrator window is in front the keypress never reaches us. Nothing about
your settings is wrong; the keystroke simply never arrives.

A **global hotkey is different**. Windows matches the combination itself and hands it to us
directly, and that still happens over an administrator window. Reading which file you have
selected works there too. So the preview itself is fine, it is only the Space key that is lost.

**The fix, if you want to keep running as administrator:** open Settings, Quick action, and bind a
hotkey to **"Quick preview the selected file"**. That key then previews the selected file
anywhere, including in an administrator window, exactly as Space does elsewhere.

**Your hotkey must include Ctrl, Alt or Shift.** That is a Windows rule, not ours: a plain
single-key shortcut is delivered like ordinary typing and gets blocked over an administrator
window in exactly the same way Space does, while a combination is handled by Windows itself and
comes through. **Ctrl+Space** is the natural pick if you want it to feel like Space.

**The other fix** is to stop running as administrator, which also brings Space back:

SageThumbs tells you this by itself, too. When an administrator window it would have served
becomes active, a tray notification explains that Space cannot work there and what to do instead.
**Click the notification** and it opens Settings so you can bind the hotkey there and then. It
repeats on a widening interval while the situation lasts, so it never silently gives up on you.

If you missed it, or you have hidden the tray icon, **run `st2k doctor` with the window open**:

```
[FAIL] Running as administrator    Everything is running as administrator, so Windows
                                   never delivers the Space keypress to us
```

The supported way around it would require the whole app to be code-signed with a purchased
certificate, and that is not something this project has. **The fix is to run Everything as a
standard user**, which voidtools also recommends:

- In Everything: Tools, Options, General, untick **Run as administrator**, tick **Everything
  Service**, then exit and restart Everything.
- Make sure the title bar does not say `[Administrator]`.

The same limitation applies anywhere else: if the window in front is running as administrator,
Space will not preview.

</details>

<details>
<summary><b>It does not work in an app's Open/Save dialog</b></summary>

Two things to check:

1. **Click a file in the list first.** When the dialog opens, the cursor is in the file-name
   box, and Space has to keep typing a space there.
2. **The app has to be 64-bit.** Old 32-bit programs are not supported, and there is no plan to
   add them.

</details>

<details>
<summary><b>Space also toggles the file's selection</b></summary>

Yes. We never swallow the key, so Explorer still receives it, which is what stops keys getting
stuck and keeps antivirus software happy. Every previewer that works this way has the same
overlap.

---

</details>

## Right-click menu

<details>
<summary><b>The menu is missing, or only the small Windows 11 menu appears</b></summary>

Windows 11 shows a short menu first, with **Show more options** at the bottom for the classic
one. SageThumbs appears in both, but the preview tile only exists on the classic menu, because
the Windows 11 menu cannot draw custom images at all.

</details>

<details>
<summary><b>I want fewer entries, or a different order</b></summary>

Settings, Right-click menu, **Edit menu items**. You can untick anything and drag entries and
their dividers into whatever order you want. The menu mirrors your list exactly.

---

</details>

## Antivirus and SmartScreen

<details>
<summary><b>Windows says "Windows protected your PC"</b></summary>

That is SmartScreen reacting to a new installer that has not built up a download reputation. If
you got the file from our GitHub releases page, click **More info**, then **Run anyway**.

</details>

<details>
<summary><b>My antivirus flagged it</b></summary>

It happens, and it is a false positive. Two honest reasons it is more likely for this program
than for most:

- It is a shell extension, so it loads into `explorer.exe`. That is normal for a thumbnail
  program and unusual for everything else.
- The Open/Save dialog preview works by briefly loading a small helper into the program that
  opened the dialog. There is no other way to read a file dialog's selection, and it is the same
  technique other preview tools use, but it does look unusual to a scanner.

If a scanner quarantines the DLL, thumbnails stop working and setup will tell you so. Allow the
install folder, then run Settings, Advanced, **Repair file associations**. Reports of specific
scanners flagging a release are welcome; we submit them.

---

</details>

## Portable version

<details>
<summary><b>What does the portable zip actually do?</b></summary>

Thumbnails and the classic right-click menu work, with no installer and no administrator rights.
Everything that is a normal program works too: Settings, Convert and Resize, Quick preview,
screenshots, OCR, the eyedropper, the folder tools and the command-line tool.

</details>

<details>
<summary><b>What does it not do?</b></summary>

Three things need registrations only an installer can make: the Explorer **preview pane**, the
**Details pane** columns, and the **Windows 11 modern menu**.

</details>

<details>
<summary><b>I moved the folder and thumbnails stopped</b></summary>

The registration records the exact path of the DLL, so moving the folder breaks it. Unregister
before you move it, then register again in the new location (Settings, Advanced).

---

</details>

## Settings, updates, uninstalling

<details>
<summary><b>Where are my settings stored?</b></summary>

Normally in the registry, under `HKCU\Software\SageThumbs2K`. The portable copy instead keeps
everything in a `SageThumbs2K.ini` next to the program, so unzipping it somewhere else leaves no
trace behind.

</details>

<details>
<summary><b>How do I uninstall?</b></summary>

Normal Windows uninstall (Settings, Apps). It removes the registrations too. For the portable
copy, unregister first (Settings, Advanced), then delete the folder.

---

</details>

## Formats

<details>
<summary><b>Which formats are supported?</b></summary>

Run `st2k formats` for the live list and the per-category breakdown. It is 300+ across images,
camera RAW, ebooks and comics, documents, audio and video.

</details>

<details>
<summary><b>Can you add format X?</b></summary>

Ask. Use **Send feedback** in the About box, or open a GitHub issue. What decides it is whether
the format can be read without a huge dependency: many "project" formats have a preview image
baked inside that we can pull out cheaply, and those are easy wins.

</details>

<details>
<summary><b>Why did some large Photoshop files not convert?</b></summary>

Older versions quietly skipped any PSD or PSB over about 270 MB when converting, with no error
and no mention in the summary beyond a smaller "converted N of M" count. That was a leftover
safety limit meant for files Explorer draws thumbnails of automatically, not files you picked
and asked to convert. It's fixed: a file you choose to convert now gets a limit sized for the
job, well past the 2 GB ceiling the .psd format has. The "Max file size (MB)" setting was never
related to this, so changing it would not have helped.

</details>

<details>
<summary><b>Why is the download this size?</b></summary>

Most of it is a trimmed copy of ImageMagick, which covers the long tail of unusual formats. The
Compact installer leaves it out and is much smaller; everything with a native decoder still
works.

---

</details>

## Licensing

<details>
<summary><b>Can I use this at work?</b></summary>

The licence is PolyForm Noncommercial 1.0.0. It is free for personal use, and commercial use
needs a commercial licence: US$49 per Windows installation, perpetual, with 12 months of
updates. Buy it at <https://checkout.connections.icu/licence/24544461-9530-4edb-84e5-4f3471876d98?slug=sagethumbs>
(card via Stripe); one seat key per installation arrives by email the moment payment completes
(the checkout page calls it a "redemption code": same thing, it starts with `esk_`).
Redeem it yourself, see below. For volume or site licences, purchase orders or bank transfer,
[request a quote](https://github.com/LunarWerxs/SageThumbs-2k/issues/new?template=licence_quote.yml).

</details>

<details>
<summary><b>How does a business licence work?</b></summary>

It comes as a seat key (`esk_...`), redeemed under **Settings ▸ Licence**. The installer asks
up front whether a copy is for personal or business use, and that answer only changes by
reinstalling, there's no toggle for it in Settings. A portable copy has no installer to ask,
and counts as business use as soon as a key is redeemed on it. A business copy has every
feature the moment it's installed, key or not, it just reminds you to add one: a notice when
you launch it, and a strip across the Settings window, on every page, that stays until you do.
Once a key is redeemed, the licence check runs quietly in the background and tolerates about a
week offline before the reminders start again.

</details>
