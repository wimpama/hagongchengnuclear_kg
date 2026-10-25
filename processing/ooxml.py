"""Bounded OOXML text extraction for DOCX/PPTX.

DOCX equations authored with Word's equation editor are stored as OMML rather
than ordinary ``w:t`` text.  This parser keeps the archive and XML limits in
the Rust caller, then converts the supported OMML structure to explicit
LaTeX.  The result is still review material: it preserves the source page
image/asset and never claims that a model or a plain-text parser verified the
equation's scientific meaning.
"""

import json
import re
import sys
import zipfile
import xml.etree.ElementTree as ET


W_NS = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
M_NS = "http://schemas.openxmlformats.org/officeDocument/2006/math"


def local_name(tag):
    return tag.rsplit("}", 1)[-1]


def children(node, name=None):
    for child in list(node):
        if name is None or local_name(child.tag) == name:
            yield child


def first(node, name):
    return next(children(node, name), None)


def attr(node, name, default=""):
    if node is None:
        return default
    for key, value in node.attrib.items():
        if local_name(key) == name:
            return value
    return default


GREEK = {
    "α": r"\alpha ", "β": r"\beta ", "γ": r"\gamma ",
    "δ": r"\delta ", "ε": r"\epsilon ", "ϵ": r"\varepsilon ",
    "ζ": r"\zeta ", "η": r"\eta ", "θ": r"\theta ",
    "ϑ": r"\vartheta ", "ι": r"\iota ", "κ": r"\kappa ",
    "λ": r"\lambda ", "μ": r"\mu ", "ν": r"\nu ",
    "ξ": r"\xi ", "ο": "o", "π": r"\pi ", "ϖ": r"\varpi ",
    "ρ": r"\rho ", "ϱ": r"\varrho ", "σ": r"\sigma ",
    "ς": r"\varsigma ", "τ": r"\tau ", "υ": r"\upsilon ",
    "φ": r"\phi ", "ϕ": r"\varphi ", "χ": r"\chi ",
    "ψ": r"\psi ", "ω": r"\omega ",
    "Γ": r"\Gamma ", "Δ": r"\Delta ", "Θ": r"\Theta ",
    "Λ": r"\Lambda ", "Ξ": r"\Xi ", "Π": r"\Pi ",
    "Σ": r"\Sigma ", "Υ": r"\Upsilon ", "Φ": r"\Phi ",
    "Ψ": r"\Psi ", "Ω": r"\Omega ",
}

SPECIAL = {
    "×": r"\times ", "∙": r"\cdot ", "·": r"\cdot ",
    "−": "-", "–": "-", "—": "-", "±": r"\pm ", "∓": r"\mp ",
    "≤": r"\leq ", "≥": r"\geq ", "≠": r"\ne ", "≈": r"\approx ",
    "∞": r"\infty ", "∂": r"\partial ", "∇": r"\nabla ",
    "∈": r"\in ", "∉": r"\notin ", "⊂": r"\subset ",
    "⊆": r"\subseteq ", "∪": r"\cup ", "∩": r"\cap ",
    "→": r"\to ", "⇒": r"\Rightarrow ", "↔": r"\leftrightarrow ",
    "∑": r"\sum ", "∏": r"\prod ", "∫": r"\int ",
    "∮": r"\oint ", "√": r"\sqrt{}", "∝": r"\propto ",
    "·": r"\cdot ", "∘": r"\circ ", "∠": r"\angle ",
    "（": "(", "）": ")", "［": "[", "］": "]", "｛": r"\{", "｝": r"\}",
    "，": ",", "：": ":", "；": ";",
    " ": " ",
}


def escape_text(value):
    """Convert one OMML text atom without changing its visible value."""
    value = value or ""
    result = []
    for char in value:
        if char in GREEK:
            result.append(GREEK[char])
        elif char in SPECIAL:
            result.append(SPECIAL[char])
        elif char in "{}_$%&#":
            result.append("\\" + char)
        else:
            result.append(char)
    text = "".join(result)
    # Word stores unit names as one atom (kg, MeV, cm).  Rendering those in
    # upright type avoids presenting a unit as a product of variables.
    stripped = value.strip()
    if len(stripped) > 1 and re.fullmatch(r"[A-Za-z]+", stripped):
        mapped = "".join(GREEK.get(c, c) for c in stripped)
        return r"\mathrm{" + mapped + "}" + (" " if value.endswith(" ") else "")
    return text


def property_value(node, property_name, default=""):
    prop = first(node, property_name)
    return attr(prop, "val", default)


def delimiter(value):
    return {
        "(": "(", ")": ")", "（": "(", "）": ")", "[": "[", "］": "]", "［": "[", "]": "]", "{": r"\{", "}": r"\}", "｛": r"\{", "｝": r"\}",
        "|": "|", "‖": r"\|", "⟨": r"\langle ", "⟩": r"\rangle ",
        "⌈": r"\lceil ", "⌉": r"\rceil ", "⌊": r"\lfloor ", "⌋": r"\rfloor ",
        "": ".",
    }.get(value, value)


def render(node):
    """Render a single OMML node to a conservative LaTeX fragment."""
    if node is None:
        return ""
    kind = local_name(node.tag)
    if kind in {"oMathPr", "oMathParaPr", "rPr", "ctrlPr", "*Pr", "argPr", "aln", "maxDist"}:
        return ""
    if kind == "t":
        return escape_text(node.text)
    if kind == "r":
        return "".join(render(child) for child in children(node))
    if kind in {"e", "num", "den", "sub", "sup", "deg", "fName", "mr", "m:mr"}:
        return "".join(render(child) for child in children(node))
    if kind == "sSup":
        return "{" + render(first(node, "e")) + "}^{" + render(first(node, "sup")) + "}"
    if kind == "sSub":
        return "{" + render(first(node, "e")) + "}_{" + render(first(node, "sub")) + "}"
    if kind == "sSubSup":
        return "{" + render(first(node, "e")) + "}_{" + render(first(node, "sub")) + "}^{" + render(first(node, "sup")) + "}"
    if kind == "f":
        return r"\frac{" + render(first(node, "num")) + "}{" + render(first(node, "den")) + "}"
    if kind == "rad":
        body = render(first(node, "e"))
        degree = first(node, "deg")
        return (r"\sqrt[" + render(degree) + "]{" + body + "}") if degree is not None else (r"\sqrt{" + body + "}")
    if kind == "d":
        prop = first(node, "dPr")
        left = delimiter(property_value(prop, "begChr"))
        right = delimiter(property_value(prop, "endChr"))
        body = "".join(render(child) for child in children(node, "e"))
        return r"\left" + left + body + r"\right" + right
    if kind == "eqArr":
        rows = [render(child) for child in children(node, "e")]
        body = r" \\ ".join(rows)
        # Word's equation-number field is serialized as ``#`` followed by a
        # hidden left/right delimiter around the number.  Keep the number as
        # a real display tag instead of exposing the field implementation.
        body = re.sub(r"\\#\\left\.(\d+)\\right\.", r"\\tag{\1}", body)
        return r"\begin{aligned}" + body + r"\end{aligned}"
    if kind == "matrix":
        rows = []
        for row in children(node, "mr"):
            rows.append(" & ".join(render(cell) for cell in children(row, "e")))
        return r"\begin{matrix}" + r" \\ ".join(rows) + r"\end{matrix}"
    if kind == "func":
        name = render(first(node, "fName"))
        body = render(first(node, "e"))
        known = {"sin", "cos", "tan", "cot", "sec", "csc", "log", "ln", "exp", "max", "min"}
        if name.strip() in known:
            return "\\" + name.strip() + "{" + body + "}"
        return r"\operatorname{" + name + "}(" + body + ")"
    if kind == "nary":
        prop = first(node, "naryPr")
        symbol = property_value(prop, "chr", "∑")
        operator = SPECIAL.get(symbol, r"\sum ")
        body = render(first(node, "e"))
        return operator + "{" + body + "}"
    if kind == "limLow":
        return "{" + render(first(node, "e")) + "}_{" + render(first(node, "lim")) + "}"
    if kind == "limUpp":
        return "{" + render(first(node, "e")) + "}^{" + render(first(node, "lim")) + "}"
    if kind == "bar":
        body = render(first(node, "e"))
        position = property_value(first(node, "barPr"), "pos", "top")
        return (r"\underline{" if position == "bot" else r"\overline{") + body + "}"
    if kind == "acc":
        body = render(first(node, "e"))
        accent = property_value(first(node, "accPr"), "chr", "ˆ")
        command = {"ˆ": r"\hat", "^": r"\hat", "~": r"\tilde", "→": r"\vec", "¯": r"\bar"}.get(accent, r"\hat")
        return command + "{" + body + "}"
    if kind == "groupChr":
        body = render(first(node, "e"))
        prop = first(node, "groupChrPr")
        position = property_value(prop, "pos", "top")
        return (r"\underbrace{" if position == "bot" else r"\overbrace{") + body + "}"
    if kind in {"box", "borderBox", "phant"}:
        return render(first(node, "e"))
    if kind == "sPre":
        return "{}_{" + render(first(node, "sub")) + "}^{" + render(first(node, "sup")) + "}" + render(first(node, "e"))
    # Unknown structural elements are concatenated conservatively.  This is
    # preferable to dropping the atom and silently changing the equation.
    return "".join(render(child) for child in children(node))


def render_math(node, display=False):
    kind = local_name(node.tag)
    if kind == "oMathPara":
        body = "".join(render(child) for child in children(node, "oMath"))
        return r"\[" + body + r"\]"
    body = render(node)
    return (r"\[" + body + r"\]") if display else (r"\(" + body + r"\)")


def render_word_node(node):
    kind = local_name(node.tag)
    if kind == "r":
        parts = []
        for child in children(node):
            child_kind = local_name(child.tag)
            if child_kind in {"t", "delText", "instrText"}:
                if child_kind != "instrText":
                    parts.append(child.text or "")
            elif child_kind == "tab":
                parts.append("\t")
            elif child_kind in {"br", "cr"}:
                parts.append("\n")
        return "".join(parts)
    if kind in {"hyperlink", "ins", "smartTag", "customXml", "sdt", "sdtContent", "fldSimple"}:
        return "".join(render_word_node(child) for child in children(node))
    if kind == "oMath":
        return render_math(node)
    if kind == "oMathPara":
        return render_math(node, display=True)
    return ""


def extract_docx(archive, name):
    xml = archive.read(name)
    if b"<!DOCTYPE" in xml.upper() or b"<!ENTITY" in xml.upper():
        raise ValueError("DTD not allowed")
    root = ET.fromstring(xml)
    paragraphs = []
    # Real DOCX files contain w:body.  The fallback keeps the bounded parser
    # useful for minimal test fixtures and harmlessly limits traversal to the
    # document root when the wrapper is omitted.
    body = next((child for child in root.iter() if local_name(child.tag) == "body"), root)
    for paragraph in children(body, "p"):
        text = "".join(render_word_node(child) for child in children(paragraph)).strip()
        if text:
            paragraphs.append(text)
    # DOCX has no reliable page boundary in OOXML without a layout engine.
    # Keep the historical single-document page contract; chunking happens in
    # Rust and the source file remains available for visual review.
    text = "\n".join(paragraphs)
    return [{"text": text, "page": None}] if text else []


def extract_pptx(archive, names):
    pages = []
    for index, name in enumerate(names, 1):
        root = ET.fromstring(archive.read(name))
        paragraphs = []
        for paragraph in root.iter():
            if local_name(paragraph.tag) != "p":
                continue
            pieces = []
            for node in paragraph.iter():
                kind = local_name(node.tag)
                if kind == "t" and node.text:
                    pieces.append(node.text)
                elif kind == "tab":
                    pieces.append("\t")
                elif kind == "br":
                    pieces.append("\n")
            text = "".join(pieces).strip()
            if text:
                paragraphs.append(text)
        pages.append({"text": "\n".join(paragraphs), "page": index})
    return pages


def main():
    path, kind = sys.argv[1:3]
    with zipfile.ZipFile(path) as archive:
        entries = archive.infolist()
        if len(entries) > 10000 or sum(item.file_size for item in entries) > 100 * 1024 * 1024:
            raise ValueError("archive expansion limit")
        if any(item.file_size > 20 * 1024 * 1024 or item.flag_bits & 1 or (item.file_size > 1024 * 1024 and item.file_size > max(item.compress_size, 1) * 1000) for item in entries):
            raise ValueError("unsafe archive")
        names = archive.namelist()
        if kind == "docx":
            if "word/document.xml" not in names or archive.getinfo("word/document.xml").file_size > 20 * 1024 * 1024:
                raise ValueError("missing document body")
            pages = extract_docx(archive, "word/document.xml")
        else:
            selected = sorted((name for name in names if re.fullmatch(r"ppt/slides/slide[0-9]+\.xml", name)), key=lambda value: int(re.search(r"([0-9]+)\.xml$", value).group(1)))
            if not selected or any(archive.getinfo(name).file_size > 20 * 1024 * 1024 for name in selected):
                raise ValueError("missing document body")
            pages = extract_pptx(archive, selected)
    output = json.dumps(pages, ensure_ascii=False).encode("utf-8")
    if len(output) > 10 * 1024 * 1024:
        raise ValueError("output limit")
    sys.stdout.buffer.write(output)


if __name__ == "__main__":
    main()
