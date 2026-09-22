"""Translate new en.toml keys into all 35 other locales with the zswarm, checked three ways.

Every new user-facing string has to land in all 36 locale files before the build will run
(`crates/build-support/src/locales.rs` refuses a key that is en-only). This is the pipeline
that did it for the upload-expiry strings on 2026-09-21, banked so the next control does not
rebuild it:

  1. build   one tool-free task per locale: the English, what each string is for, and ten
             strings that locale ALREADY has, so the worker keeps its words for "upload",
             "Copy", the ellipsis and the dash.
  2. review  a second model reads each locale's result and proposes fixes.
  3. judge   a third pass decides, per proposed fix, whether the ORIGINAL was really wrong.
  4. apply   validates every locale mechanically and appends one block per file.

Measured on that first run: 35/35 first-pass results passed the mechanical checks, and the
reviewer (gpt-oss-120b) proposed 77 fixes of which roughly half were WRONG (Dutch "u" for
hours changed to English "h", Polish "godz." to "h", "Copy copies" collapsed into "Copia
copia"). The judge (deepseek-flash) confirmed 11 of 70. So: never apply review fixes blind.
Read the judge's confirmed list (step 3 prints it), keep the ones that are right, add matching
siblings (a fixed minutes abbreviation must change in every dur_* string that uses it), and
pass them to `apply` as an overrides file.

    python i18n_tasks.py build  <keys.json> <workdir>     # keys.json: {"key": "what it is"}
    python zswarm.py run --model deepseek-flash --tools none --concurrency 40 \
        --out <workdir>/i18n-job.json <workdir>/tasks.json
    python i18n_tasks.py review <workdir>
    python zswarm.py run --model groq-gpt-oss-120b --tools none \
        --out <workdir>/review-job.json <workdir>/review-tasks.json
    python i18n_tasks.py judge  <workdir>
    python zswarm.py run --model deepseek-flash --tools none \
        --out <workdir>/judge-job.json <workdir>/judge-tasks.json
    python i18n_tasks.py confirmed <workdir>              # what the judge would change
    python i18n_tasks.py apply  <workdir> [--overrides o.json] [--dry]

`o.json` is {"<locale>": {"<key>": "<replacement>"}}. `apply` refuses (writes nothing) unless
every locale has every key, the same {placeholders} as English, the ellipsis / blank lines /
"SageThumbs 2K" the English has, no key already in the file, and no BOM. Then run
`pwsh scripts/check-locale-keys.ps1`. Stdlib only; a workdir under `tmp/` is gitignored.
"""

import json
import pathlib
import re
import sys
import tomllib

REPO = pathlib.Path(__file__).resolve().parents[3]
LOC = REPO / "assets" / "locales"
PH = re.compile(r"\{[a-z_:0-9]+\}")
REFERENCE = [
    "menu_upload", "btn_copy", "btn_close", "btn_edit_upload_hosts", "tip_edit_upload_hosts",
    "up_caption_file", "up_done_one", "up_busy_many", "tray_settings", "licence_state_trial",
]
NAMES = {
    "ar": "Arabic", "bg": "Bulgarian", "cs": "Czech", "da": "Danish", "de": "German",
    "el": "Greek", "es": "Spanish", "fa": "Persian (Farsi)", "fi": "Finnish",
    "fil": "Filipino", "fr": "French", "he": "Hebrew", "hi": "Hindi", "hr": "Croatian",
    "hu": "Hungarian", "id": "Indonesian", "it": "Italian", "ja": "Japanese",
    "ko": "Korean", "ms": "Malay", "nb": "Norwegian Bokmal", "nl": "Dutch", "pl": "Polish",
    "pt-BR": "Brazilian Portuguese", "ro": "Romanian", "ru": "Russian", "sk": "Slovak",
    "sl": "Slovenian", "sv": "Swedish", "th": "Thai", "tr": "Turkish", "uk": "Ukrainian",
    "vi": "Vietnamese", "zh-CN": "Simplified Chinese", "zh-TW": "Traditional Chinese",
}


def load_toml(code: str) -> dict:
    return tomllib.loads((LOC / f"{code}.toml").read_text(encoding="utf-8"))


def read_json(path: pathlib.Path):
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path: pathlib.Path, value) -> None:
    path.write_text(json.dumps(value, ensure_ascii=False, indent=1), encoding="utf-8")


def cmd_build(keys_file: str, workdir: str) -> None:
    context = read_json(pathlib.Path(keys_file))
    work = pathlib.Path(workdir)
    work.mkdir(parents=True, exist_ok=True)
    en = load_toml("en")
    missing = [k for k in context if k not in en]
    if missing:
        sys.exit(f"not in en.toml yet: {missing}")
    keys = list(context)
    tasks = []
    for code, name in NAMES.items():
        loc = load_toml(code)
        ref = "\n".join(
            f"- {k}: {loc[k]!r} / {en[k]!r}" for k in REFERENCE if k in loc and k in en
        )
        items = [{"key": k, "english": en[k], "what_it_is": context[k]} for k in keys]
        prompt = (
            f"Translate UI strings for SageThumbs 2K, a free Windows image-thumbnail and "
            f"screenshot tool, into {name} (locale code {code}).\n\n"
            "Rules:\n"
            "- Keep every {placeholder} EXACTLY, same spelling, each exactly once; you may move "
            "them to fit the grammar.\n"
            "- Match the wording this locale ALREADY uses (examples below) for the app's terms "
            "and for punctuation such as the ellipsis and em dash.\n"
            "- Plain, friendly, short UI language; no marketing tone.\n"
            "- Keep any line breaks (including blank lines) where the English has them.\n"
            "- Return the text only, no surrounding quotes.\n\n"
            f"Existing {name} strings in this app (key: {name} / English):\n{ref}\n\n"
            "Strings to translate (JSON):\n"
            + json.dumps(items, ensure_ascii=False, indent=1)
            + f"\n\nSubmit an object with exactly these {len(keys)} keys, each mapped to its "
            "translation."
        )
        schema = {
            "type": "object",
            "properties": {k: {"type": "string"} for k in keys},
            "required": keys,
            "additionalProperties": False,
        }
        tasks.append({"id": code, "prompt": prompt, "schema": schema})
    write_json(work / "tasks.json", tasks)
    print(f"{len(tasks)} tasks -> {work / 'tasks.json'}")


def cmd_review(workdir: str) -> None:
    work = pathlib.Path(workdir)
    tasks = read_json(work / "tasks.json")
    first = read_json(work / "i18n-job.json")["results"]
    out = []
    for t in tasks:
        keys = t["schema"]["required"]
        prompt = (
            "You are reviewing a translation of UI strings. Below is the translation brief, then "
            "the translation produced. Judge ONLY real problems: a wrong or misleading meaning, "
            "a grammatical error, a lost or altered {placeholder}, an abbreviation a native "
            "speaker would not recognise, or wording that contradicts the app's existing terms "
            "shown in the brief. Do NOT rewrite for taste. For each real problem give the full "
            "corrected string. If everything is fine, return an empty fixes list.\n\n"
            "=== BRIEF ===\n" + t["prompt"] + "\n\n=== TRANSLATION PRODUCED ===\n"
            + json.dumps(first[t["id"]]["data"], ensure_ascii=False, indent=1)
        )
        fix = {
            "type": "object",
            "properties": {
                "key": {"type": "string", "enum": keys},
                "problem": {"type": "string"},
                "corrected": {"type": "string"},
            },
            "required": ["key", "problem", "corrected"],
        }
        schema = {
            "type": "object",
            "properties": {"fixes": {"type": "array", "items": fix}},
            "required": ["fixes"],
        }
        out.append({"id": t["id"], "prompt": prompt, "schema": schema})
    write_json(work / "review-tasks.json", out)
    print(f"{len(out)} review tasks")


def proposed_fixes(work: pathlib.Path):
    """(locale, key) -> the reviewer's first correction that differs from the original."""
    first = read_json(work / "i18n-job.json")["results"]
    review = read_json(work / "review-job.json")["results"]
    fixes = {}
    for code, r in sorted(review.items()):
        for f in (r.get("data") or {}).get("fixes") or []:
            original = first[code]["data"].get(f["key"], "")
            if (code, f["key"]) not in fixes and f["corrected"].strip() != original.strip():
                fixes[(code, f["key"])] = (original, f)
    return fixes


def cmd_judge(workdir: str) -> None:
    work = pathlib.Path(workdir)
    en = load_toml("en")
    brief = {t["id"]: t["prompt"] for t in read_json(work / "tasks.json")}
    out = []
    for (code, key), (original, f) in proposed_fixes(work).items():
        prompt = (
            "A UI string was translated, then a reviewer proposed a correction. Decide whether "
            "the ORIGINAL translation contains a real error a native speaker would object to "
            "(wrong meaning, grammar error, lost placeholder, unrecognisable abbreviation, or a "
            "term that contradicts the app's existing wording in the brief). Stylistic "
            "preference is NOT an error. The correction must itself be correct, keep every "
            "{placeholder}, and not duplicate words. Answer use_correction ONLY if the original "
            "is really wrong AND the correction is right; otherwise keep_original.\n\n"
            f"=== BRIEF ===\n{brief[code]}\n\n=== THE STRING ===\nkey: {key}\n"
            f"English: {en[key]!r}\nORIGINAL: {original!r}\n"
            f"Claimed problem: {f['problem']}\nCorrection: {f['corrected']!r}\n"
        )
        schema = {
            "type": "object",
            "properties": {
                "verdict": {"type": "string", "enum": ["keep_original", "use_correction"]},
                "reason": {"type": "string"},
            },
            "required": ["verdict", "reason"],
        }
        out.append({"id": f"{code}__{key}", "prompt": prompt, "schema": schema})
    write_json(work / "judge-tasks.json", out)
    print(f"{len(out)} judge tasks")


def cmd_confirmed(workdir: str) -> None:
    work = pathlib.Path(workdir)
    fixes = proposed_fixes(work)
    judged = read_json(work / "judge-job.json")["results"]
    for tid, r in sorted(judged.items()):
        d = r.get("data") or {}
        if d.get("verdict") != "use_correction":
            continue
        code, key = tid.split("__", 1)
        # Gone when the translation has changed since the judge ran (it now matches the fix,
        # or was overridden by hand) - nothing left to decide for that string.
        if (code, key) not in fixes:
            print(f"{code} {key}\n  (already changed since the judge ran)")
            continue
        original, f = fixes[(code, key)]
        print(f"{code} {key}\n  was: {original}\n  fix: {f['corrected']}\n  why: {d['reason']}")


def toml_basic(s: str) -> str:
    out = []
    for c in s:
        if c == "\\":
            out.append("\\\\")
        elif c == '"':
            out.append('\\"')
        elif c == "\n":
            out.append("\\n")
        elif c == "\t":
            out.append("\\t")
        elif ord(c) < 0x20 or ord(c) == 0x7F:
            out.append(f"\\u{ord(c):04X}")
        else:
            out.append(c)
    return '"' + "".join(out) + '"'


def problems(data: dict, en: dict, existing: dict) -> list[str]:
    errs = []
    for k, en_val in en.items():
        v = data.get(k)
        if not isinstance(v, str) or not v.strip():
            errs.append(f"{k}: missing/empty")
            continue
        v = v.replace("\\n", "\n")  # a worker that wrote the escape instead of the break
        data[k] = v
        if sorted(PH.findall(v)) != sorted(PH.findall(en_val)):
            errs.append(f"{k}: placeholders {PH.findall(v)} != {PH.findall(en_val)}")
        if k in existing:
            errs.append(f"{k}: already in the file")
        if v.strip() != v:
            errs.append(f"{k}: leading/trailing whitespace")
        if en_val.endswith("…") and not v.endswith("…"):
            errs.append(f"{k}: lost its ellipsis")
        if v.count("\n\n") < en_val.count("\n\n"):
            errs.append(f"{k}: lost a blank line")
        if "SageThumbs 2K" in en_val and "SageThumbs 2K" not in v:
            errs.append(f"{k}: lost the product name")
    extra = set(data) - set(en)
    if extra:
        errs.append(f"unexpected keys {sorted(extra)}")
    return errs


def cmd_apply(workdir: str, overrides: str | None, dry: bool) -> int:
    work = pathlib.Path(workdir)
    tasks = read_json(work / "tasks.json")
    results = read_json(work / "i18n-job.json")["results"]
    manual = read_json(pathlib.Path(overrides)) if overrides else {}
    keys = list(tasks[0]["schema"]["required"])
    en_all = load_toml("en")
    en = {k: en_all[k] for k in keys}
    bad, plan = {}, {}
    for t in tasks:
        code = t["id"]
        r = results.get(code) or {}
        if r.get("status") != "ok" or not isinstance(r.get("data"), dict):
            bad[code] = [f"status {r.get('status')}"]
            continue
        data = dict(r["data"]) | manual.get(code, {})
        path = LOC / f"{code}.toml"
        raw = path.read_bytes()
        errs = problems(data, en, tomllib.loads(raw.decode("utf-8")))
        if raw.startswith(b"\xef\xbb\xbf"):
            errs.append("file has a BOM")
        if errs:
            bad[code] = errs
        else:
            plan[code] = (path, raw, data)
    if bad:
        for code, errs in bad.items():
            print(f"FAIL {code}: " + "; ".join(errs))
        print(f"{len(bad)} locale(s) failed; nothing written")
        return 1
    for code, (path, raw, data) in plan.items():
        eol = "\r\n" if b"\r\n" in raw else "\n"
        text = raw.decode("utf-8")
        if not text.endswith("\n"):
            text += eol
        block = [""] + [f"{k} = {toml_basic(data[k])}" for k in keys]
        text += eol.join(block) + eol
        parsed = tomllib.loads(text)  # must still parse, every key exactly once
        assert all(parsed[k] == data[k] for k in keys), code
        if not dry:
            path.write_bytes(text.encode("utf-8"))
    print(f"{'validated' if dry else 'wrote'} {len(plan)} locales")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) < 3:
        print(__doc__)
        return 2
    cmd, rest = argv[1], argv[2:]
    if cmd == "build" and len(rest) == 2:
        cmd_build(rest[0], rest[1])
    elif cmd == "review":
        cmd_review(rest[0])
    elif cmd == "judge":
        cmd_judge(rest[0])
    elif cmd == "confirmed":
        cmd_confirmed(rest[0])
    elif cmd == "apply":
        ov = rest[rest.index("--overrides") + 1] if "--overrides" in rest else None
        return cmd_apply(rest[0], ov, "--dry" in rest)
    else:
        print(__doc__)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
