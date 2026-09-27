"""The locales, the worker prompts and the JSON/TOML helpers every i18n_tasks.py step shares."""

import json
import pathlib
import re
import tomllib


REPO = pathlib.Path(__file__).resolve().parents[3]
LOC = REPO / "assets" / "locales"
PH = re.compile(r"\{[a-z_:0-9]+\}")
_SECTIONS = re.split(
    r"^<!-- prompt: (\w+) -->\n",
    (pathlib.Path(__file__).with_name("i18n_prompts.md")).read_text(encoding="utf-8"),
    flags=re.M,
)
# Each section minus the one line break that separates it from the next marker.
PROMPTS = {name: text[:-1] for name, text in zip(_SECTIONS[1::2], _SECTIONS[2::2])}
# TOML basic-string escapes; any other control character becomes \uXXXX.
TOML_ESCAPES = {"\\": "\\\\", '"': '\\"', "\n": "\\n", "\t": "\\t"}
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
