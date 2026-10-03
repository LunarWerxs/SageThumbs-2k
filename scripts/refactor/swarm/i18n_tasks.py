"""Translate new en.toml keys into all 35 other locales with HSwarm, checked three ways.

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
    hswarm run --model deepseek-flash --tools none --concurrency 40 \
        --out <workdir>/i18n-job.json <workdir>/tasks.json
    python i18n_tasks.py review <workdir>
    hswarm run --model groq-gpt-oss-120b --tools none \
        --out <workdir>/review-job.json <workdir>/review-tasks.json
    python i18n_tasks.py judge  <workdir>
    hswarm run --model deepseek-flash --tools none \
        --out <workdir>/judge-job.json <workdir>/judge-tasks.json
    python i18n_tasks.py confirmed <workdir>              # what the judge would change
    python i18n_tasks.py apply  <workdir> [--overrides o.json] [--dry]

`o.json` is {"<locale>": {"<key>": "<replacement>"}}. `apply` refuses (writes nothing) unless
every locale has every key, the same {placeholders} as English, the ellipsis / blank lines /
"SageThumbs 2K" the English has, no key already in the file, and no BOM. Then run
`pwsh scripts/check-locale-keys.ps1`. Stdlib only; a workdir under `tmp/` is gitignored.

The three worker prompts are str.format templates in `i18n_prompts.md` beside this script,
one `<!-- prompt: NAME -->` section each.
"""

import json
import pathlib
import sys

from i18n_apply import cmd_apply
from i18n_locales import NAMES, PROMPTS, REFERENCE, load_toml, read_json, write_json
from i18n_review import cmd_confirmed, cmd_judge, cmd_review


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
        prompt = PROMPTS["build"].format(
            name=name,
            code=code,
            ref=ref,
            items=json.dumps(items, ensure_ascii=False, indent=1),
            count=len(keys),
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
