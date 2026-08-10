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

### Thumbnails are not showing up at all

Run this in a terminal and read what it says:

```
st2k doctor
```

It is read-only, it walks the entire chain (Windows' own thumbnail switches, our four
registrations, whether the DLL actually loads, whether another program has taken over the file
types, your settings, and a live decode test), and it prints a specific fix under anything that
is wrong. Paste its output into a bug report and you will get a much faster answer.

The three most common causes it finds:

1. **Windows is set to "Always show icons, never thumbnails".** File Explorer, View, Options,
   View tab, untick it. A group policy can also force this on managed machines.
2. **Another program took the file types.** Photo tools often claim `.jpg`, `.png` and RAW on
   install. Fix it in Settings, Advanced, **Repair file associations**, which re-registers every
   format you have enabled and then clears the thumbnail cache.
3. **Your antivirus quarantined the DLL during setup.** See
   [Antivirus and SmartScreen](#antivirus-and-smartscreen).

### Some thumbnails appear, others stay as blank icons

Windows caches thumbnails aggressively, and a file it failed on once stays failed. Settings,
Advanced, **Rebuild thumbnail cache** clears Windows' cache and restarts Explorer.

If it is a whole format rather than scattered files, check that format is ticked in Settings,
File types.

### Thumbnails work in a folder, but the file is blank in OneDrive

If the file is online-only, there are no bytes on the disk to read. Windows will not download a
file just to draw a thumbnail, and neither will we. Mark the folder **Always keep on this
device** and the thumbnails appear.

### A file shows a thumbnail in Explorer but a plain caption in the right-click menu

That is deliberate, not a bug. The little preview tile inside the right-click menu is drawn
**inside `explorer.exe` itself**, so it only runs the cheap, safe decoders. Video frames, PDF
pages and anything that needs the bundled ImageMagick are skipped there and show the file name
and size instead. Those same files still thumbnail normally in the folder and in the preview
pane, which run in their own isolated process where a slow or hostile file cannot hurt Explorer.

---

## Press Space to preview

### Nothing happens when I press Space

It is **off by default**. Turn it on in Settings, Quick preview. It runs a small background
helper, the same one the hotkeys use.

### It works in Explorer but not in Everything

You need **Everything 1.5 or newer**. Version 1.4 does not publish which result is selected, so
there is nothing for us to read, and Space stays a space there.

Also click a result first. While your cursor is still in Everything's search box, Space types a
space, which is what it should do.

### It does not work when Everything runs as administrator

**This one cannot be fixed from our side, and it is not going to be.** Windows deliberately
stops a normal program from seeing keys typed into a program running as administrator. Our
background helper is a normal program, so when an administrator window is in front, the keypress
never reaches us. Nothing about the setting is wrong; the keystroke simply never arrives.

The supported way around it would require the whole app to be code-signed with a purchased
certificate, and that is not something this project has. **The fix is to run Everything as a
standard user**, which voidtools also recommends:

- In Everything: Tools, Options, General, untick **Run as administrator**, tick **Everything
  Service**, then exit and restart Everything.
- Make sure the title bar does not say `[Administrator]`.

The same limitation applies anywhere else: if the window in front is running as administrator,
Space will not preview.

### It does not work in an app's Open/Save dialog

Two things to check:

1. **Click a file in the list first.** When the dialog opens, the cursor is in the file-name
   box, and Space has to keep typing a space there.
2. **The app has to be 64-bit.** Old 32-bit programs are not supported, and there is no plan to
   add them.

### Space also toggles the file's selection

Yes. We never swallow the key, so Explorer still receives it, which is what stops keys getting
stuck and keeps antivirus software happy. Every previewer that works this way has the same
overlap.

---

## Right-click menu

### The menu is missing, or only the small Windows 11 menu appears

Windows 11 shows a short menu first, with **Show more options** at the bottom for the classic
one. SageThumbs appears in both, but the preview tile only exists on the classic menu, because
the Windows 11 menu cannot draw custom images at all.

### I want fewer entries, or a different order

Settings, Right-click menu, **Edit menu items**. You can untick anything and drag entries and
their dividers into whatever order you want. The menu mirrors your list exactly.

---

## Antivirus and SmartScreen

### Windows says "Windows protected your PC"

That is SmartScreen reacting to a new installer that has not built up a download reputation. If
you got the file from our GitHub releases page, click **More info**, then **Run anyway**.

### My antivirus flagged it

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

## Portable version

### What does the portable zip actually do?

Thumbnails and the classic right-click menu work, with no installer and no administrator rights.
Everything that is a normal program works too: Settings, Convert and Resize, Quick preview,
screenshots, OCR, the eyedropper, the folder tools and the command-line tool.

### What does it not do?

Three things need registrations only an installer can make: the Explorer **preview pane**, the
**Details pane** columns, and the **Windows 11 modern menu**.

### I moved the folder and thumbnails stopped

The registration records the exact path of the DLL, so moving the folder breaks it. Unregister
before you move it, then register again in the new location (Settings, Advanced).

---

## Settings, updates, uninstalling

### Where are my settings stored?

Normally in the registry, under `HKCU\Software\SageThumbs2K`. The portable copy instead keeps
everything in a `SageThumbs2K.ini` next to the program, so unzipping it somewhere else leaves no
trace behind.

### How do I uninstall?

Normal Windows uninstall (Settings, Apps). It removes the registrations too. For the portable
copy, unregister first (Settings, Advanced), then delete the folder.

---

## Formats

### Which formats are supported?

Run `st2k formats` for the live list and the per-category breakdown. It is 300+ across images,
camera RAW, ebooks and comics, documents, audio and video.

### Can you add format X?

Ask. Use **Send feedback** in the About box, or open a GitHub issue. What decides it is whether
the format can be read without a huge dependency: many "project" formats have a preview image
baked inside that we can pull out cheaply, and those are easy wins.

### Why is the download this size?

Most of it is a trimmed copy of ImageMagick, which covers the long tail of unusual formats. The
Compact installer leaves it out and is much smaller; everything with a native decoder still
works.

---

## Licensing

### Can I use this at work?

The licence is PolyForm Noncommercial 1.0.0. It is free for personal use, and commercial use
needs a separate licence. Ask through **Send feedback** or a GitHub issue.
