<!-- prompt: build -->
Translate UI strings for SageThumbs 2K, a free Windows image-thumbnail and screenshot tool, into {name} (locale code {code}).

Rules:
- Keep every {{placeholder}} EXACTLY, same spelling, each exactly once; you may move them to fit the grammar.
- Match the wording this locale ALREADY uses (examples below) for the app's terms and for punctuation such as the ellipsis and em dash.
- Plain, friendly, short UI language; no marketing tone.
- Keep any line breaks (including blank lines) where the English has them.
- Return the text only, no surrounding quotes.

Existing {name} strings in this app (key: {name} / English):
{ref}

Strings to translate (JSON):
{items}

Submit an object with exactly these {count} keys, each mapped to its translation.
<!-- prompt: review -->
You are reviewing a translation of UI strings. Below is the translation brief, then the translation produced. Judge ONLY real problems: a wrong or misleading meaning, a grammatical error, a lost or altered {{placeholder}}, an abbreviation a native speaker would not recognise, or wording that contradicts the app's existing terms shown in the brief. Do NOT rewrite for taste. For each real problem give the full corrected string. If everything is fine, return an empty fixes list.

=== BRIEF ===
{brief}

=== TRANSLATION PRODUCED ===
{translation}
<!-- prompt: judge -->
A UI string was translated, then a reviewer proposed a correction. Decide whether the ORIGINAL translation contains a real error a native speaker would object to (wrong meaning, grammar error, lost placeholder, unrecognisable abbreviation, or a term that contradicts the app's existing wording in the brief). Stylistic preference is NOT an error. The correction must itself be correct, keep every {{placeholder}}, and not duplicate words. Answer use_correction ONLY if the original is really wrong AND the correction is right; otherwise keep_original.

=== BRIEF ===
{brief}

=== THE STRING ===
key: {key}
English: {english!r}
ORIGINAL: {original!r}
Claimed problem: {problem}
Correction: {corrected!r}

