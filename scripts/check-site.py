#!/usr/bin/env python3
"""Validate the dependency-free GitHub Pages source tree."""

from __future__ import annotations

from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import urlsplit


ROOT = Path(__file__).resolve().parents[1]
SITE = ROOT / "site"
PUBLIC_ORIGIN = "https://spawnr-cli.dev"


class Document(HTMLParser):
    def __init__(self, path: Path) -> None:
        super().__init__(convert_charrefs=True)
        self.path = path
        self.ids: set[str] = set()
        self.references: list[str] = []
        self.canonicals: list[str] = []
        self.errors: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        values = dict(attrs)
        element_id = values.get("id")
        if element_id:
            if element_id in self.ids:
                self.errors.append(f"duplicate id {element_id!r}")
            self.ids.add(element_id)

        for attribute in ("href", "src"):
            reference = values.get(attribute)
            if reference:
                self.references.append(reference)

        if tag == "link" and values.get("rel") == "canonical":
            href = values.get("href")
            if href:
                self.canonicals.append(href)


def local_target(document: Path, reference: str) -> tuple[Path, str] | None:
    parsed = urlsplit(reference)
    if parsed.scheme or parsed.netloc:
        return None
    if parsed.path.startswith("/"):
        target = SITE / parsed.path.lstrip("/")
    elif parsed.path:
        target = document.parent / parsed.path
    else:
        target = document
    if parsed.path.endswith("/") or not parsed.path:
        target /= "index.html" if target.is_dir() or parsed.path.endswith("/") else ""
    return target.resolve(), parsed.fragment


def main() -> None:
    failures: list[str] = []
    documents: dict[Path, Document] = {}
    html_paths = sorted(SITE.rglob("*.html"))
    if not html_paths:
        failures.append("site contains no HTML documents")

    for path in html_paths:
        parser = Document(path)
        try:
            parser.feed(path.read_text(encoding="utf-8"))
            parser.close()
        except Exception as error:  # HTMLParser errors carry useful context.
            failures.append(f"{path.relative_to(ROOT)}: cannot parse: {error}")
            continue
        documents[path.resolve()] = parser
        failures.extend(
            f"{path.relative_to(ROOT)}: {error}" for error in parser.errors
        )
        expected = PUBLIC_ORIGIN + (
            "/" if path == SITE / "index.html" else f"/{path.parent.relative_to(SITE)}/"
        )
        if parser.canonicals != [expected]:
            failures.append(
                f"{path.relative_to(ROOT)}: canonical must be exactly {expected!r}"
            )

    for path, parser in documents.items():
        for reference in parser.references:
            resolved = local_target(path, reference)
            if resolved is None:
                continue
            target, fragment = resolved
            if not target.exists():
                failures.append(
                    f"{path.relative_to(ROOT)}: missing local reference {reference!r}"
                )
                continue
            if fragment and target.suffix == ".html":
                target_document = documents.get(target)
                if target_document is not None and fragment not in target_document.ids:
                    failures.append(
                        f"{path.relative_to(ROOT)}: missing fragment {reference!r}"
                    )

    landing = (SITE / "index.html").read_text(encoding="utf-8")
    if f"curl -fsSL {PUBLIC_ORIGIN}/install.sh | sh" not in landing:
        failures.append("landing page does not contain the canonical installer command")
    if "https://spawnr.dev" in "\n".join(
        path.read_text(encoding="utf-8")
        for path in SITE.rglob("*")
        if path.is_file() and path.suffix in {".html", ".xml", ".txt", ".js", ".css"}
    ):
        failures.append("site still references the retired spawnr.dev origin")

    if failures:
        raise SystemExit("site validation failed:\n- " + "\n- ".join(failures))
    print(f"validated {len(documents)} HTML documents and their local references")


if __name__ == "__main__":
    main()
