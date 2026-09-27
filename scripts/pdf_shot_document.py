"""The demo document make-pdf-shot.py renders, and the one find term it must hold exactly once."""

from __future__ import annotations

import sys


# The Ctrl+F term the shot demonstrates. It must appear EXACTLY ONCE in the document, or the
# find bar reads "1/3" instead of "1/1" and the screenshot stops showing a clean single hit.
# `assert_single_hit` enforces that against the real text below rather than trusting it.
FIND_TERM = "andromeda"

# The page the viewer is scrolled to. Page 3 is where FIND_TERM lives, so the caption's page
# counter and the find bar agree - the point of the shot is that the search JUMPED here.
FIND_PAGE = 3


# ---- The demo document -------------------------------------------------------------------
#
# A fictional visitor guide for a fictional observatory: ordinary prose, one table per content
# page, one bullet list. The screenshot is selling the VIEWER (continuous scroll, the page
# thumbnail strip, find-and-jump), so the document has to read like a real multi-page document
# and otherwise get out of the way.

PAGES: list[list[tuple]] = [
    [
        ("kicker", "SECTION ONE"),
        ("h1", "Dark Sky Field Guide"),
        ("rule", None),
        ("body", "The ridge has been dark for as long as anyone here has been counting, and "
                 "the guide you are holding exists to keep it that way. Read it before your "
                 "first visit; most of it is about light, and the rest is about gates."),
        ("body", "The observatory is run by volunteers. Nobody is paid to be here at two in "
                 "the morning, so the rules below are the ones that keep the site usable "
                 "rather than the ones a lawyer would write."),
        ("h2", "Before you arrive"),
        ("bullets", [
            "Check the cloud forecast the afternoon before, not the week before.",
            "Charge everything at home. There is no mains power at the pads.",
            "Bring more layers than you think. The ridge runs cold after midnight.",
            "Tell someone which pad you are on if you are observing alone.",
        ]),
        ("body", "New visitors are welcome on any open night. If it is your first time, park "
                 "in the lower field and walk up; the upper track is reserved for people "
                 "unloading heavy mounts."),
    ],
    [
        ("kicker", "SECTION TWO"),
        ("h1", "Getting here and setting up"),
        ("rule", None),
        ("body", "The site sits at the end of a single track road. The last two miles are "
                 "unlit by design, so approach on sidelights and expect to meet people "
                 "walking in the dark."),
        ("h2", "Pad assignments"),
        ("table", {
            "head": ["Pad", "Surface", "Best for"],
            "rows": [
                ["North one", "Concrete", "Heavy equatorial mounts and permanent piers"],
                ["North two", "Concrete", "Long exposure imaging, shielded from the road"],
                ["East", "Gravel", "Visual observing and quick setups"],
                ["Meadow", "Grass", "Wide field cameras and anyone with a tripod"],
            ],
        }),
        ("body", "Pads are first come, first served, except on public nights when the two "
                 "north pads are held for the outreach scopes until nine."),
    ],
    [
        ("kicker", "SECTION THREE"),
        ("h1", "Autumn highlights"),
        ("rule", None),
        ("body", "Autumn is the season the ridge is known for. The Milky Way still arches "
                 "overhead at dusk, the fog blanks the valley lights, and the first of the "
                 "winter clusters rise before midnight."),
        ("table", {
            "head": ["Object", "Type", "How to find it"],
            "rows": [
                # The single occurrence of FIND_TERM in the whole document.
                ["Andromeda Galaxy", "Galaxy",
                 "Naked eye from the pads; follow the arrow of Cassiopeia's deeper V"],
                ["Double Cluster", "Open clusters",
                 "Binoculars; midway between Perseus and Cassiopeia"],
                ["Pleiades", "Open cluster",
                 "Rises over the east marker after 22:00 in September"],
                ["Fomalhaut", "Star", "The lone bright star skimming the southern rim"],
            ],
        }),
        ("h2", "House rules, the short version"),
        ("bullets", [
            "Red light past the gate, always.",
            "Walk the rim path, not across the meadow: dew soaked grass and tripod legs.",
            "Generators are welcome at the east pads only, and off by midnight.",
            "Leave the gate as you found it. The herd two fields down is real.",
        ]),
        ("body", "The guide's seasonal tables continue on the winter and spring pages, and "
                 "the last page lists the volunteer schedule. Clear skies."),
        ("footer", "Ridgeline Community Observatory  -  Visitor guide, fourth revision"),
    ],
    [
        ("kicker", "SECTION FOUR"),
        ("h1", "Winter, spring, and the volunteer rota"),
        ("rule", None),
        ("body", "Winter is the clearest and the cruelest. Transparency on the ridge in "
                 "January is the best it gets all year, and so is the wind chill, so the "
                 "warm room stays unlocked from November through February."),
        ("h2", "Who to ask"),
        ("bullets", [
            "Gate code and pad bookings: the duty volunteer, by radio on channel four.",
            "Outreach nights and school groups: the education team, in writing, please.",
            "Anything broken, dark, or missing: the site warden, immediately.",
        ]),
        ("body", "The rota is posted inside the warm room door and refreshed at the start of "
                 "each season. If you have been coming for a year and have not yet taken a "
                 "shift, this is the paragraph that is about you."),
        ("footer", "Ridgeline Community Observatory  -  Visitor guide, fourth revision"),
    ],
]


def blocks_text(blocks: list[tuple]) -> str:
    """Every visible string in one page's blocks. `rule` carries no text (payload is None),
    which is why this is a plain loop rather than a conditional expression - the first cut
    was the latter and fed None to the table branch."""
    out: list[str] = []
    for kind, payload in blocks:
        if kind in ("kicker", "h1", "h2", "body", "footer"):
            out.append(payload)
        elif kind == "bullets":
            out.extend(payload)
        elif kind == "table":
            out.extend(payload["head"])
            for row in payload["rows"]:
                out.extend(row)
    return "\n".join(out)


def document_text() -> str:
    """Every visible string in PAGES - what the viewer's find will search."""
    return "\n".join(blocks_text(blocks) for blocks in PAGES)


def assert_single_hit() -> None:
    """FIND_TERM must occur exactly once, on FIND_PAGE - see FIND_TERM's comment."""
    hits = document_text().lower().count(FIND_TERM)
    if hits != 1:
        sys.exit(f"FIND_TERM {FIND_TERM!r} occurs {hits} times in the demo document; the shot "
                 f"needs exactly one so the find bar reads 1/1")
    if FIND_TERM not in blocks_text(PAGES[FIND_PAGE - 1]).lower():
        sys.exit(f"FIND_TERM {FIND_TERM!r} is not on page {FIND_PAGE}; the caption's page "
                 f"counter and the find bar would disagree")
