"""Bounded PDF text extraction. Page images remain the authority for math layout.

Invoked in an isolated subprocess by Rust. No OCR, formula inference, embedded
scripts, attachment extraction, or external resource requests are performed.
"""
import json
import sys


def extract(path, max_pages):
    # Bound the helper independently of the API on deployed Linux containers.
    # The Rust parent also enforces an output budget and a wall-clock deadline.
    try:
        import resource
    except ImportError:
        pass  # Windows uses the parent's deadline and the stream/output caps.
    else:
        resource.setrlimit(resource.RLIMIT_AS, (512 * 1024**2, 512 * 1024**2))
        resource.setrlimit(resource.RLIMIT_CPU, (45, 45))

    try:
        from pypdf import PdfReader
        from pypdf import filters
    except ImportError:
        return {"error": "missing_dependency"}

    # pypdf limits Flate expansion too; tighten its per-stream default for the
    # small shared server. Never accept unbounded decompression as a fallback.
    filters.ZLIB_MAX_OUTPUT_LENGTH = 25 * 1024**2
    try:
        reader = PdfReader(path, strict=False)
        if reader.is_encrypted:
            return {"error": "invalid_document"}
        if not 1 <= len(reader.pages) <= max_pages:
            return {"error": "page_limit"}
        pages = []
        output_bytes = 0
        content_bytes = 0
        for number, page in enumerate(reader.pages, 1):
            contents = page.get_contents()
            size = len(contents.get_data()) if contents is not None else 0
            content_bytes += size
            if size > 25 * 1024**2 or content_bytes > 100 * 1024**2:
                return {"error": "content_limit"}
            text = (page.extract_text() or "").strip()
            # Strict encoding prevents silently replacing mathematical glyphs.
            output_bytes += len(text.encode("utf-8", errors="strict"))
            if output_bytes > 9 * 1024**2:
                return {"error": "content_limit"}
            # Include blank and scanned pages to keep text/image page alignment.
            pages.append({"page": number, "text": text})
        return {"pages": pages}
    except (MemoryError, OverflowError):
        return {"error": "content_limit"}
    except UnicodeError:
        return {"error": "invalid_text"}
    except Exception:
        return {"error": "invalid_document"}


if __name__ == "__main__":
    result = extract(sys.argv[1], min(max(int(sys.argv[2]), 1), 500))
    data = json.dumps(result, ensure_ascii=False).encode("utf-8", errors="strict")
    if len(data) > 10 * 1024**2:
        data = b'{"error":"content_limit"}'
    sys.stdout.buffer.write(data)
