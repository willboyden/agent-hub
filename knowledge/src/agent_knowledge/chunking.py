"""Fixed-window character chunking with overlap, preferring to break at paragraph/sentence/space boundaries."""
from __future__ import annotations


def chunk_text(text: str, size: int = 1000, overlap: int = 200) -> list[str]:
    if size < 50:
        raise ValueError("chunk size must be >= 50")
    if overlap < 0 or overlap >= size:
        raise ValueError("overlap must be >= 0 and smaller than the chunk size")
    text = text.strip()
    if not text:
        return []
    out: list[str] = []
    start, n = 0, len(text)
    while start < n:
        end = min(start + size, n)
        if end < n:
            # look for a natural break in the last 20% of the window
            floor = start + int(size * 0.8)
            for sep in ("\n\n", "\n", ". ", " "):
                cut = text.rfind(sep, floor, end)
                if cut != -1:
                    end = cut + len(sep)
                    break
        piece = text[start:end].strip()
        if piece:
            out.append(piece)
        if end >= n:
            break
        start = max(end - overlap, start + 1)
    return out
