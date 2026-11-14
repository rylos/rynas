"""The interface's texts for the audits: the English source of the dictionaries (src/i18n.rs,
src/modules/*/i18n.rs) and a language's translation files (assets/i18n/<lang>/, docs/i18n.md)."""
import glob
import os
import re

UI = os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../crates/rynas-ui")
STR = r'"((?:[^"\\]|\\.)*)"'


def english():
    """key -> English (Rust literal contents, as written)."""
    out = {}
    for f in [os.path.join(UI, "src/i18n.rs")] + sorted(glob.glob(os.path.join(UI, "src/modules/*/i18n.rs"))):
        out.update(re.findall(STR + r"\s*=>\s*" + STR + r";", open(f, encoding="utf-8").read()))
    return out


def translation(lang):
    """key -> text of every translation file of `lang` (empty for English)."""
    out = {}
    for f in sorted(glob.glob(os.path.join(UI, "assets/i18n", lang, "*.txt"))):
        for line in open(f, encoding="utf-8").read().splitlines():
            if not line.startswith("#") and "\t" in line:
                k, v = line.split("\t", 1)
                out[k] = v
    return out


def english_only(lang):
    """English texts that differ in `lang` and are not the translated text of another key
    ("Temperature", "Status"): seeing one in `lang` is a sign of untranslated text."""
    tr = translation(lang if lang != "en" else "it")
    pairs = [(en, tr.get(k, en)) for k, en in english().items()]
    translated = {t for _, t in pairs}
    return sorted({en for en, t in pairs if en != t and en not in translated and re.search(r"[A-Za-z]{3}", en) and "{" not in en})
