# SPDX-License-Identifier: AGPL-3.0-only
"""Writes tests/fixtures/tokenizer/ids.json: the token IDs that the reference
Python tokenizer (the `tokenizers` package, built with the onig regex engine)
gives for a fixed list of texts, from the pinned tokenizer.json.

The real-model test compares them with the Rust tokenizer, which is built with
fancy-regex instead.

Usage:
  uv run --no-project --with tokenizers==0.23.3 python -I \
      scripts/make-tokenizer-fixture.py <model folder>/tokenizer.json \
      tests/fixtures/tokenizer/ids.json
"""

import hashlib
import json
import sys

import tokenizers

TOKENIZER_SHA256 = "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4"

TEXTS = [
    "",
    "hello",
    "Hello, World!",
    "title: Maya · 2026-11-23 | text: Maya prefers window seats travel entity/maya",
    "task: question answering | query: where does Maya like to sit?",
    "title: none · 2026-01-02 | text: s\n\nlong text after two newlines",
    "  leading and trailing spaces  ",
    "tabs\tand\nnew\r\nlines",
    "multiple     spaces    between   words",
    "numbers 1 12 123 1234 12345 3.14159 -42 1,000,000",
    "dates 2026-11-25T10:15:30.123Z and times 09:30",
    "punctuation!?.,;:'\"()[]{}<>/\\|@#$%^&*_+=~`",
    "contractions: don't, can't, I'm, we've, they'll, it's",
    "CamelCaseWords snake_case_words kebab-case-words",
    "URLs https://example.com/path?query=1&x=y and email a.b@example.org",
    "fn main() { println!(\"{}\", 1 + 2); }",
    "café naïve résumé über straße",
    "é combining accent",
    "日本語のテキストです。",
    "中文文本，包含标点。",
    "한국어 문장입니다",
    "العربية نص",
    "Русский текст",
    "हिन्दी पाठ",
    "emoji \U0001f600 \U0001f44d\U0001f3fd \U0001f468‍\U0001f469‍\U0001f467 ❤️",
    "zero​width and non breaking",
    "repeated letters aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "<start_of_turn>user special-looking <pad> <eos> text",
    "Mixed: The 3 quick brown foxes (à la française) jumped 2x over 日本!",
]


def main() -> None:
    if len(sys.argv) != 3:
        sys.exit("usage: make-tokenizer-fixture.py <tokenizer.json> <out>")
    source, out = sys.argv[1], sys.argv[2]
    with open(source, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    if digest != TOKENIZER_SHA256:
        sys.exit(f"{source} is not the pinned tokenizer.json (SHA-256 {digest})")
    tokenizer = tokenizers.Tokenizer.from_file(source)
    tokenizer.no_truncation()
    cases = [
        {"text": text, "ids": tokenizer.encode(text, add_special_tokens=True).ids}
        for text in TEXTS
    ]
    fixture = {
        "tokenizer_sha256": TOKENIZER_SHA256,
        "reference": f"tokenizers {tokenizers.__version__} (Python)",
        "cases": cases,
    }
    # One case per line keeps the file short and its diffs readable.
    head = json.dumps({k: v for k, v in fixture.items() if k != "cases"}, ensure_ascii=False)
    lines = ",\n".join(json.dumps(c, ensure_ascii=False) for c in cases)
    with open(out, "w", encoding="utf-8") as f:
        f.write(head[:-1] + ', "cases": [\n' + lines + "\n]}\n")


if __name__ == "__main__":
    main()
