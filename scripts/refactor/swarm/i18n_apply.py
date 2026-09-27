"""Step 4 of i18n_tasks.py: validate every locale mechanically, then append one block per file."""

import pathlib
import tomllib

from i18n_locales import LOC, PH, TOML_ESCAPES, load_toml, read_json


def toml_escape(c: str) -> str:
    if c in TOML_ESCAPES:
        return TOML_ESCAPES[c]
    return f"\\u{ord(c):04X}" if ord(c) < 0x20 or ord(c) == 0x7F else c


def toml_basic(s: str) -> str:
    return '"' + "".join(map(toml_escape, s)) + '"'


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
