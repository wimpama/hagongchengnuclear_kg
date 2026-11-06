"""Inspect linked PPTX media without fetching URLs or opening local paths.

The original remains untouched. On explicit consent, produce a disposable copy
without linked media; ordinary navigation hyperlinks and embedded fallbacks
are retained. Reports contain counts and display page numbers, never URLs.
"""
import json
import posixpath
import sys
import zipfile
import xml.etree.ElementTree as ET

REL = "http://schemas.openxmlformats.org/package/2006/relationships"
R = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
P = "http://schemas.openxmlformats.org/presentationml/2006/main"
A = "http://schemas.openxmlformats.org/drawingml/2006/main"
ET.register_namespace("p", P)
ET.register_namespace("a", A)
ET.register_namespace("r", R)


def parse(data):
    if b"<!DOCTYPE" in data.upper() or b"<!ENTITY" in data.upper():
        raise ValueError("DTD not allowed")
    return ET.fromstring(data)


def rel_path(owner):
    folder, name = posixpath.split(owner)
    return f"{folder}/_rels/{name}.rels"


def target(owner, value):
    result = posixpath.normpath(posixpath.join(posixpath.dirname(owner), value))
    if result.startswith("../"):
        raise ValueError("invalid package path")
    return result.lstrip("/")


def inspect(source, output, skip=False):
    with zipfile.ZipFile(source) as archive:
        entries = archive.infolist()
        if len(entries) > 10000 or sum(e.file_size for e in entries) > 100 * 1024 * 1024:
            raise ValueError("archive limit")
        if any(e.file_size > 20 * 1024 * 1024 or e.flag_bits & 1 for e in entries):
            raise ValueError("unsafe archive")
        names = set(archive.namelist())
        if len(names) != len(entries):
            raise ValueError("duplicate package paths")
        relationships = {}

        def rels(owner):
            if owner not in relationships:
                key = rel_path(owner)
                relationships[owner] = list(parse(archive.read(key))) if key in names else []
            return relationships[owner]

        pres = parse(archive.read("ppt/presentation.xml"))
        slide_rels = {e.get("Id"): e for e in rels("ppt/presentation.xml")}
        owners = {}
        # Follow presentation order, not slideN.xml numbering.
        for page, node in enumerate(pres.findall(f".//{{{P}}}sldId"), 1):
            edge = slide_rels.get(node.get(f"{{{R}}}id"))
            if edge is None or edge.get("TargetMode") == "External":
                raise ValueError("missing slide")
            slide = target("ppt/presentation.xml", edge.get("Target", ""))
            todo, visited = [slide], set()
            while todo:
                owner = todo.pop()
                if owner in visited:
                    continue
                visited.add(owner)
                owners.setdefault(owner, set()).add(page)
                for relation in rels(owner):
                    kind = relation.get("Type", "").rsplit("/", 1)[-1]
                    if relation.get("TargetMode") != "External" and kind in {"slideLayout", "slideMaster"}:
                        todo.append(target(owner, relation.get("Target", "")))

        linked = []
        replacements = {}
        for owner, pages in owners.items():
            xml = parse(archive.read(owner))
            external = {}
            for relation in rels(owner):
                if relation.get("TargetMode") != "External":
                    continue
                kind = relation.get("Type", "").rsplit("/", 1)[-1]
                if kind == "hyperlink":
                    continue
                rid = relation.get("Id")
                if not rid:
                    raise ValueError("invalid relation")
                used = any(value == rid and key.startswith(f"{{{R}}}")
                           for node in xml.iter() for key, value in node.attrib.items())
                if not used:
                    continue
                external[rid] = kind
                linked.append({"pages": sorted(pages), "kind": kind})
            if not external or not skip:
                continue
            parents = {child: parent for parent in xml.iter() for child in parent}
            remove = set()
            for node in list(xml.iter()):
                attrs = [key for key, value in node.attrib.items()
                         if key.startswith(f"{{{R}}}") and value in external]
                if not attrs:
                    continue
                # A linked picture may also carry a complete embedded copy.
                if node.tag == f"{{{A}}}blip" and node.get(f"{{{R}}}embed") not in external and node.get(f"{{{R}}}embed"):
                    for key in attrs:
                        del node.attrib[key]
                    continue
                ancestors = [node]
                while ancestors[-1] in parents:
                    ancestors.append(parents[ancestors[-1]])
                # Remove the whole picture/frame. Leaving an empty p:pic after
                # stripping blipFill produces another renderer placeholder.
                selected = next((e for e in ancestors if e.tag in {f"{{{P}}}pic", f"{{{P}}}graphicFrame"}), None)
                if selected is None:
                    selected = next((e for e in ancestors if e.tag.rsplit("}", 1)[-1] in {"blipFill", "buBlip", "videoFile", "audioFile", "media"}), None)
                if selected is None or selected is xml:
                    raise ValueError("unsupported external resource")
                remove.add(selected)
            for node in remove:
                if node in parents and node in list(parents[node]):
                    parents[node].remove(node)
            replacements[owner] = ET.tostring(xml, encoding="utf-8", xml_declaration=True)
            key = rel_path(owner)
            rel_root = parse(archive.read(key))
            for node in list(rel_root):
                if node.get("Id") in external:
                    rel_root.remove(node)
            replacements[key] = ET.tostring(rel_root, encoding="utf-8", xml_declaration=True)

        report = {"count": len(linked), "pages": sorted({p for e in linked for p in e["pages"]}),
                  "kinds": sorted({e["kind"] for e in linked})}
        if skip:
            with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED) as cleaned:
                for entry in entries:
                    cleaned.writestr(entry.filename, replacements.get(entry.filename, archive.read(entry)))
        return report


if __name__ == "__main__":
    try:
        report = inspect(sys.argv[1], sys.argv[2], sys.argv[3] == "skip")
        print(json.dumps({"externalResources": report}))
    except Exception:
        print(json.dumps({"error": "PPTX 外部资源检测失败，请检查文件或导出为 PDF 后重试"}))
