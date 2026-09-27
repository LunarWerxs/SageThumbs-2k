"""Steps 2 and 3 of i18n_tasks.py: the review and judge task files, and what the judge confirmed."""

import json
import pathlib

from i18n_locales import PROMPTS, load_toml, read_json, write_json


def cmd_review(workdir: str) -> None:
    work = pathlib.Path(workdir)
    tasks = read_json(work / "tasks.json")
    first = read_json(work / "i18n-job.json")["results"]
    out = []
    for t in tasks:
        keys = t["schema"]["required"]
        prompt = PROMPTS["review"].format(
            brief=t["prompt"],
            translation=json.dumps(first[t["id"]]["data"], ensure_ascii=False, indent=1),
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
        prompt = PROMPTS["judge"].format(
            brief=brief[code],
            key=key,
            english=en[key],
            original=original,
            problem=f["problem"],
            corrected=f["corrected"],
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
