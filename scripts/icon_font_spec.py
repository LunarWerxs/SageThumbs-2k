"""What the bundled icon font holds, and how its outlines are normalized (see build-icon-font.py)."""


# The face is RENAMED rather than left as "Material Symbols Outlined" on purpose: the font is
# loaded privately (AddFontResourceEx + FR_PRIVATE) and a distinct name means a user who has
# their own Material Symbols installed cannot have theirs picked instead of ours, and ours
# cannot leak into other applications' font lists.
FACE_NAME = "SageThumbs2K Icons"

# Instance the variable font at one fixed point. `wght=400` matches the weight the old Segoe
# glyphs drew at, and `opsz=24` is the size these toolbars actually render near.
INSTANCE = {"FILL": 0, "GRAD": 0, "opsz": 24, "wght": 400}

# Every glyph the three toolbars draw, as (upstream Material name, OUR codepoint).
#
# ★ THE CODEPOINTS ARE THE APP'S EXISTING SEGOE ONES, NOT MATERIAL'S. That is the whole trick:
# by placing each Material glyph at the codepoint the Rust side already asks for, NOTHING in
# `preview::paint::btn_glyph`, `preview::transport` or `screenshot::toolbar::button_glyph`
# changes, and the OS-font fallback chain keeps working against the same single table. Remap
# here, never in three Rust files.
#
# Upstream Material codepoints are resolved by NAME at build time from the `.codepoints` file
# that ships beside the font, so a name is the stable identifier and this table never has to
# track an upstream renumbering.
GLYPHS = [
    # caption toolbar (preview/paint.rs)
    ("format_list_bulleted", 0xE8FD),  # Toc, the Markdown outline toggle
    ("image", 0xEB9F),                 # MdImages, load web images
    ("code", 0xE943),                  # Source, view source
    ("chevron_left", 0xE76B),          # PdfPrev
    ("chevron_right", 0xE76C),         # PdfNext
    ("push_pin", 0xE718),              # Pin, unpinned (outline)
    ("content_copy", 0xE8C8),          # Copy, shared with the screenshot editor
    # 0xE8D2 is the "A": the preview's OCR button and the screenshot editor's Text tool BOTH
    # use it today, and in Segoe they are the same glyph, so one entry preserves both exactly.
    ("text_fields", 0xE8D2),
    ("info", 0xE946),                  # Info
    ("upload", 0xE898),                # Upload
    ("open_in_new", 0xE8A7),           # Open
    ("open_in_browser", 0xE7AC),       # OpenWith
    ("print", 0xE749),                 # Print, the current image or PDF page
    ("close", 0xE711),                 # Close, shared with the screenshot editor
    ("settings", 0xE713),              # Settings, jumps to Settings > Quick preview
    # The theme toggle draws ONE of these, whichever it would switch TO: a sun while the
    # viewer is dark, a moon while it is light. Both must exist or the button goes blank in
    # one of its two states. The codepoints are Segoe's Brightness / QuietHours, which are a
    # sun and a moon there too, so the OS-font fallback chain still reads correctly.
    ("light_mode", 0xE706),            # Theme, switch to light
    ("dark_mode", 0xE708),             # Theme, switch to dark
    # video transport (preview/transport.rs)
    ("play_arrow", 0xE768),
    ("pause", 0xE769),
    ("skip_previous", 0xE892),
    ("skip_next", 0xE893),
    ("volume_up", 0xE767),
    ("volume_off", 0xE74F),
    ("repeat", 0xE8EE),
    ("swap_horiz", 0xE8AB),
    # screenshot editor (screenshot/toolbar.rs)
    ("edit", 0xE70F),                  # Pen
    ("ink_highlighter", 0xE7E6),       # Highlight
    ("colorize", 0xEF3C),              # Eyedropper
    ("drag_pan", 0xE7C2),              # Move
    ("undo", 0xE7A7),
    ("redo", 0xE7A6),
    ("save", 0xE74E),
    ("cloud_upload", 0xE753),          # Upload (cloud)
]

# The pinned state needs a FILLED pin, which in a variable Material font is the SAME glyph at
# `FILL=1`. A single static instance cannot hold both, so the filled pin is built from a second
# instance and grafted in - at the app's existing "pinned" codepoint.
PIN_MATERIAL_NAME = "push_pin"
PIN_FILLED_OUT = 0xE840

# ---------------------------------------------------------------------------
# OPTICAL NORMALIZATION - why the outlines get scaled instead of shipped as-is
# ---------------------------------------------------------------------------
# Material draws every icon inside a 24dp grid but lets each one use as much of that grid as
# its own shape wants: `info` is a circle filling 20dp, `close` is an X inset to 14dp. Read one
# at a time that is a deliberate optical choice. Read as a ROW of eight in a 38px toolbar it is
# just uneven - measured off a real capture of the Quick preview caption, the ink boxes ranged
# from 9 px (close) to 14 px (push_pin), a 1.55x spread, which is what a user sees and calls
# "the icons are different sizes".
#
# So each glyph is scaled about the grid centre until its LONGEST side reaches TARGET. The
# clamp is the whole subtlety: a uniform scale takes the stroke weight with it, so pushing a
# thin mark like `chevron_right` (12dp tall) all the way to 20dp would make it the BOLDEST
# thing on the bar while fixing its size - trading one kind of unevenness for a worse one.
# MAX_SCALE stops short of that, which leaves the genuinely small marks (chevrons, skip) a
# little smaller than the solid ones, exactly as they should be.
NORM_TARGET = 800  # font units at 960 upm = 20dp, the largest extent Material itself uses
NORM_MAX_SCALE = 1.30  # never bolden a thin mark past this to make it "match"
NORM_MIN_SCALE = 0.92  # and never shrink a wide one to nothing
NORM_CENTER = (480, 480)  # the 24dp grid's centre in font units - scale about this, not the bbox

# Codepoints that MUST come out at the same scale as each other. The pin is one button in two
# states: an outline pin and a filled one. They are separate glyphs from separate instances, so
# nothing but this makes them agree - and a pin that changed SIZE when you pinned the window
# would read as the toolbar twitching.
SCALE_GROUPS = [(0xE718, PIN_FILLED_OUT)]
