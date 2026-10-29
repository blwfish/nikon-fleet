#!/usr/bin/env python3
"""Generate sdk-runtime/maid-labels.json from the Nikon Remote SDK documents.

    scripts/extract-maid-labels.py /path/to/S-SDKZ-200BF-ALLIN [--out FILE] [--pdftotext PATH]

Reads, from the SDK folder (python3 standard library plus `pdftotext` from poppler for the PDF):

  * Module/*/HeaderFile/Maid3.h      the names of the base capabilities (the `eNkMAIDCapability` enum,
                                     `X = 1`, then the next names, `Y = X + 2` jumps and all,
                                     evaluated as a C compiler would)
  * Module/Document/English/MAID3-COMMON(E).pdf
                                     the names of an enumeration's values, where a capability's
                                     section says `One of eNkMAIDX` and lists `N: name` lines

and writes one JSON file, by default sdk-runtime/maid-labels.json, which is gitignored: the SDK is
licensed to the person who ran this, so nothing derived from it belongs in the repository. The
program works without the file; `nikon-fleet show` then prints the raw numbers and says so.

PDF parsing is conservative. A capability gets value names only when its section names an
enumeration and lists numbered constants unambiguously; everything else is left out and recorded in
`unparsed` with a reason. A list is never completed by guessing: a missing number, a repeated
number, a label that may continue on the next line, entries that resume after other text, bit flags,
and an enumeration name that two sections define with different constants all leave the capability
unparsed. Disagreements inside the document that do not stop a list from being read (the prose
after it names a constant the list does not have) are recorded as `doc_warnings`, never fixed.

Exit status: 0 written, 2 the SDK folder or a file in it is missing, pdftotext is missing or failed,
or Maid3.h could not be evaluated.
"""

import argparse
import datetime
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys

EXTRACTOR_VERSION = 1

PDF_RELATIVE = os.path.join("Module", "Document", "English", "MAID3-COMMON(E).pdf")
HEADER_GLOB_DIRS = ("Mac", "Win", "Windows")
PDF_NAME = "MAID3-COMMON(E).pdf"

# Where poppler is usually installed when it is not on PATH (a sandboxed shell lacks /opt/homebrew/bin).
PDFTOTEXT_FALLBACKS = ("/opt/homebrew/bin/pdftotext", "/usr/local/bin/pdftotext", "/usr/bin/pdftotext")

# The `ulType` words that make a capability's data an integer a list of names can describe.
ENUM_TYPES = {"kNkMAIDCapType_Enum", "kNkMAIDCapType_Unsigned", "kNkMAIDArrayType_Unsigned"}

# Reasons a capability is left unparsed. Closed: a consumer may match on them.
NOT_ENUM_TYPE = "not_enum_type"
NO_ENUM_STATEMENT = "no_enum_statement"
NO_NUMBERED_LIST = "no_numbered_list"
AMBIGUOUS_ENUM_NAME = "ambiguous_enum_name"
DUPLICATE_VALUE = "duplicate_value"
BIT_FLAGS = "bit_flags"
LIST_MAY_BE_INCOMPLETE = "list_may_be_incomplete"
VALUE_OUT_OF_RANGE = "value_out_of_range"
ENUM_NAME_CONFLICT = "enum_name_conflict"
DUPLICATE_SECTION = "duplicate_section"
NAME_MISMATCH = "name_mismatch"
REASONS = (
    NOT_ENUM_TYPE,
    NO_ENUM_STATEMENT,
    NO_NUMBERED_LIST,
    AMBIGUOUS_ENUM_NAME,
    DUPLICATE_VALUE,
    BIT_FLAGS,
    LIST_MAY_BE_INCOMPLETE,
    VALUE_OUT_OF_RANGE,
    ENUM_NAME_CONFLICT,
    DUPLICATE_SECTION,
    NAME_MISMATCH,
)


class ExtractError(Exception):
    """Something the person running this can fix (a missing file or tool); the message says what."""


# ── Maid3.h: the base capability names ────────────────────────────────────

def _strip_comments(text):
    text = re.sub(r"/\*.*?\*/", " ", text, flags=re.S)
    return re.sub(r"//[^\n]*", "", text)


def _tokens(expr):
    out = []
    pos = 0
    pattern = re.compile(r"\s*(0[xX][0-9A-Fa-f]+|\d+|[A-Za-z_]\w*|<<|[-+*|()])")
    while pos < len(expr):
        if expr[pos:].strip() == "":
            break
        m = pattern.match(expr, pos)
        if not m:
            raise ExtractError("cannot read %r in the enum expression %r" % (expr[pos:pos + 10], expr))
        out.append(m.group(1))
        pos = m.end()
    return out


def eval_c_expr(expr, names):
    """Evaluate an integer constant expression of the shapes a C enum uses: numbers (decimal or
    hex), earlier enumerators, `+ - * | << ( )`. Anything else is an error, never a guess."""
    toks = _tokens(expr)
    pos = [0]

    def peek():
        return toks[pos[0]] if pos[0] < len(toks) else None

    def take():
        t = peek()
        pos[0] += 1
        return t

    def primary():
        t = take()
        if t is None:
            raise ExtractError("the enum expression %r ends early" % expr)
        if t == "(":
            v = bit_or()
            if take() != ")":
                raise ExtractError("unbalanced parenthesis in %r" % expr)
            return v
        if t == "-":
            return -primary()
        if re.fullmatch(r"0[xX][0-9A-Fa-f]+", t):
            return int(t, 16)
        if t.isdigit():
            return int(t, 10)
        if t in names:
            return names[t]
        raise ExtractError("%r in the enum expression %r is not an earlier enumerator" % (t, expr))

    def mul():
        v = primary()
        while peek() == "*":
            take()
            v *= primary()
        return v

    def add():
        v = mul()
        while peek() in ("+", "-"):
            v = v + mul() if take() == "+" else v - mul()
        return v

    def shift():
        v = add()
        while peek() == "<<":
            take()
            v <<= add()
        return v

    def bit_or():
        v = shift()
        while peek() == "|":
            take()
            v |= shift()
        return v

    value = bit_or()
    if peek() is not None:
        raise ExtractError("unexpected %r in the enum expression %r" % (peek(), expr))
    return value


def parse_c_enum(text, enum_name):
    """The (name, value) pairs of `enum <enum_name> { ... };` in a C header, in order."""
    m = re.search(r"\benum\s+%s\s*\{(.*?)\}\s*;" % re.escape(enum_name), _strip_comments(text), flags=re.S)
    if not m:
        raise ExtractError("enum %s not found in the header" % enum_name)
    body = m.group(1)
    if "#" in body:
        raise ExtractError("enum %s contains a preprocessor line; not evaluated" % enum_name)
    names = {}
    out = []
    next_value = 0
    for item in body.split(","):
        item = item.strip()
        if not item:
            continue
        if "=" in item:
            name, expr = (s.strip() for s in item.split("=", 1))
            value = eval_c_expr(expr, names)
        else:
            name, value = item, next_value
        if not re.fullmatch(r"[A-Za-z_]\w*", name):
            raise ExtractError("cannot read the enumerator %r in %s" % (name, enum_name))
        if name in names:
            raise ExtractError("enumerator %s is defined twice in %s" % (name, enum_name))
        names[name] = value
        out.append((name, value))
        next_value = value + 1
    return out


BASE_PREFIX = "kNkMAIDCapability_"
VENDOR_BASE = "VendorBase"


def base_capabilities(header_text):
    """{"1": "NameOfOne", ...}: the base capabilities by number. `VendorBase` marks where the
    vendor capabilities start; it is not one."""
    out = {}
    for name, value in parse_c_enum(header_text, "eNkMAIDCapability"):
        if not name.startswith(BASE_PREFIX):
            raise ExtractError("%s in eNkMAIDCapability does not start with %s" % (name, BASE_PREFIX))
        short = name[len(BASE_PREFIX):]
        if short == VENDOR_BASE:
            continue
        key = str(value)
        if key in out:
            raise ExtractError("capability number %s is both %s and %s" % (key, out[key], short))
        out[key] = short
    return out


# ── MAID3-COMMON(E).pdf: the names of an enumeration's values ─────────────

CAP_RE = re.compile(r"^\s*Capability\s+kNkMAIDCapability_(\w+)\s*$")
HEAD_RE = re.compile(r"^\s*(\d+\.\d+)\s+([A-Za-z]\w*)")
DATA_RE = re.compile(r"^\s*Data\b")
RESULT_RE = re.compile(r"^\s*Result Code\b")
TYPE_RE = re.compile(r"(kNkMAID(?:Cap|Array)Type)_\s*(\w+)")
ENUM_STATEMENT_RE = re.compile(r"\b[Oo]ne\s+(?:of\s+)?(?:the\s+)?(e[A-Za-z0-9_]+)\b")
ENTRY_RE = re.compile(r"^\s*(0[xX][0-9A-Fa-f]+|\d+)\s*:\s*(\S.*?)\s*$")
# A constant the prose names in quotes, `“17: U3(User Mode3)”`.
PROSE_ENTRY_RE = re.compile(r"[“\"](\d+)\s*:\s*([A-Za-z][^”\"]*?)\s*[”\"]")
HEADING_LOOKBACK = 8
UINT32_MAX = 0xFFFFFFFF


def _indent(line):
    return len(line) - len(line.lstrip())


def _norm(label):
    return re.sub(r"\s+", "", label).lower()


def _int(text):
    return int(text, 16) if text[:2] in ("0x", "0X") else int(text, 10)


def capability_sections(text):
    """The capability sections of the document: [{"capability", "section", "title", "types", "lines"}],
    where `capability` is the name on the `Capability` line, `title` the name in the section's heading
    and `lines` run from the `Capability` line to the next section."""
    lines = [l.replace("\f", "") for l in text.split("\n")]
    caps = [i for i, l in enumerate(lines) if CAP_RE.match(l)]
    out = []
    for n, i in enumerate(caps):
        end = caps[n + 1] if n + 1 < len(caps) else len(lines)
        section = title = None
        for j in range(i - 1, max(i - HEADING_LOOKBACK, -1), -1):
            h = HEAD_RE.match(lines[j])
            if h:
                section, title = h.group(1), h.group(2)
                break
        body = lines[i:end]
        # The heading of the next section sits just above its `Capability` line, past the end of this one.
        stop = len(body)
        for k in range(1, len(body)):
            if HEAD_RE.match(body[k]) and not DATA_RE.match(body[k]):
                stop = k
                break
        body = body[:stop]
        types = []
        for l in body[:10]:
            if "ulOperations" in l:
                break
            types.extend("%s_%s" % pair for pair in TYPE_RE.findall(l))
        out.append({"capability": CAP_RE.match(lines[i]).group(1), "section": section, "title": title, "types": types, "lines": body})
    return out


def _data_block(lines):
    start = next((i for i, l in enumerate(lines) if DATA_RE.match(l)), None)
    if start is None:
        return None
    end = len(lines)
    for i in range(start, len(lines)):
        if RESULT_RE.match(lines[i]):
            end = i
            break
    return lines[start:end]


def _read_entries(block, statement_line):
    """The consecutive `N: name` lines after `statement_line`. Returns (entries, problem) where
    problem is None or (reason, text)."""
    i = statement_line + 1
    while i < len(block) and not block[i].strip():
        i += 1
    entries = []
    indents = []
    while i < len(block):
        m = ENTRY_RE.match(block[i])
        if not m:
            break
        entries.append((_int(m.group(1)), m.group(2), m.group(1)))
        indents.append(_indent(block[i]))
        i += 1
    if not entries:
        return [], (NO_NUMBERED_LIST, "no `N: name` lines follow the statement")
    rest = [l for l in block[i:] if l.strip()]
    if rest and _indent(rest[0]) >= min(indents):
        return [], (LIST_MAY_BE_INCOMPLETE, "the line after the list, %r, is indented like the list: a name may continue onto it" % rest[0].strip())
    resumed = next((l for l in rest if ENTRY_RE.match(l)), None)
    if resumed:
        return [], (LIST_MAY_BE_INCOMPLETE, "numbered lines resume after other text (%r)" % resumed.strip())
    return entries, None


def _doc_warnings(block, statement_line, entries):
    """Constants the prose names in quotes that the list disagrees with. Recorded, not fixed."""
    by_value = {v: label for v, label, _ in entries}
    warnings = []
    for line in block[statement_line + 1:]:
        if ENTRY_RE.match(line):
            continue
        for value_text, label in PROSE_ENTRY_RE.findall(line):
            value = int(value_text)
            if value not in by_value:
                warnings.append("the text names %d: %s, which the list does not have" % (value, label))
            elif _norm(by_value[value]) != _norm(label):
                warnings.append("the text names %d: %s, the list has %d: %s" % (value, label, value, by_value[value]))
    return warnings


def parse_section(sec):
    """(result, None) with the capability's entry, or (None, (reason, text)) when it is left unparsed."""
    cap = sec["capability"]
    block = _data_block(sec["lines"])
    kinds = sec["types"]
    if not any(t in ENUM_TYPES for t in kinds):
        return None, (NOT_ENUM_TYPE, "ulType is %s" % (", ".join(kinds) or "not stated"))
    if block is None:
        return None, (NO_ENUM_STATEMENT, "the section has no Data part")
    statements = [(i, m.group(1)) for i, l in enumerate(block) for m in ENUM_STATEMENT_RE.finditer(l)]
    names = sorted({n for _, n in statements})
    if not names:
        return None, (NO_ENUM_STATEMENT, "the Data part does not say `one of eXxx`")
    if len(names) > 1:
        return None, (AMBIGUOUS_ENUM_NAME, "the Data part names several enumerations: %s" % ", ".join(names))
    line = statements[0][0]
    entries, problem = _read_entries(block, line)
    if problem:
        return None, problem
    values = [v for v, _, _ in entries]
    if len(set(values)) != len(values):
        dup = sorted({v for v in values if values.count(v) > 1})
        return None, (DUPLICATE_VALUE, "the list repeats %s" % ", ".join(str(d) for d in dup))
    if any(v > UINT32_MAX for v in values):
        return None, (VALUE_OUT_OF_RANGE, "a value exceeds 32 bits")
    if len(values) >= 3 and all(v > 0 and v & (v - 1) == 0 for v in values):
        return None, (BIT_FLAGS, "every value is a single bit: a reading is a combination of them, not one of them")
    result = {
        "enum": names[0],
        "values": {str(v): label for v, label, _ in entries},
        "provenance": "%s section %s" % (PDF_NAME, sec["section"] or "?"),
    }
    warnings = _doc_warnings(block, line, entries)
    if warnings:
        result["doc_warnings"] = warnings
    return result, None


def _conflicting(a, b):
    return any(k in b["values"] and b["values"][k] != v for k, v in a["values"].items())


def resolve_enum_name_conflicts(parsed):
    """Remove entries whose enumeration name another section defines with a different label for
    the same number. A document that gives one enumeration name three unrelated meanings has
    copy-pasted the name; the section whose capability is that enumeration's own keeps it and the
    others are dropped. With no such owner, all the conflicting sections are dropped.
    Returns {capability: (reason, text)} for the entries removed."""
    removed = {}
    by_enum = {}
    for cap, entry in parsed.items():
        by_enum.setdefault(entry["enum"], []).append(cap)
    for enum, caps in by_enum.items():
        if len(caps) < 2:
            continue
        conflicted = {c for c in caps for d in caps if c != d and _conflicting(parsed[c], parsed[d])}
        if not conflicted:
            continue
        owners = [c for c in caps if c.lower() in enum.lower()]
        owner = owners[0] if len(owners) == 1 else None
        for c in sorted(conflicted):
            if c == owner:
                continue
            others = ", ".join(sorted(d for d in caps if d != c))
            removed[c] = (ENUM_NAME_CONFLICT, "%s is also the enumeration of %s with different constants%s" % (
                enum, others, "" if owner is None else " (%s is its own capability and keeps it)" % owner))
    return removed


def resolve_name(sec, known_names):
    """(name, warning, problem): the capability a section documents. The `Capability` line and the
    heading usually agree; the document has sections where they do not (a made-up name under
    another capability's heading). Which one is the typo is not read off the document: it is
    settled only when exactly one of the two is a capability MaidLayer.config defines, and
    otherwise the section is left unparsed."""
    cap, title = sec["capability"], sec["title"]
    if title is None or title == cap:
        return cap, None, None
    if known_names is not None:
        by_line, by_title = BASE_PREFIX + cap in known_names, BASE_PREFIX + title in known_names
        if by_line != by_title:
            chosen = cap if by_line else title
            return chosen, "the heading says %s, the Capability line says %s; MaidLayer.config defines only %s" % (title, cap, chosen), None
    return cap, None, (NAME_MISMATCH, "the heading says %s, the Capability line says %s%s" % (
        title, cap, "" if known_names is not None else " (no MaidLayer.config given to settle it)"))


def parse_pdf_text(text, known_names=None):
    """(capabilities, unparsed, sections_seen) from the text `pdftotext -layout` made of the PDF.
    `known_names`, when given, is the set of `kNkMAIDCapability_X` names MaidLayer.config defines."""
    sections = capability_sections(text)
    parsed = {}
    unparsed = []
    seen = {}
    for sec in sections:
        cap, warning, problem = resolve_name(sec, known_names)
        if problem:
            unparsed.append({"capability": cap, "section": sec["section"], "reason": problem[0], "detail": problem[1]})
            continue
        if cap in seen:
            unparsed.append({"capability": cap, "section": sec["section"], "reason": DUPLICATE_SECTION,
                             "detail": "the document has two sections for it (%s and %s)" % (seen[cap], sec["section"])})
            parsed.pop(cap, None)
            continue
        seen[cap] = sec["section"]
        result, problem = parse_section(sec)
        if result is not None:
            if warning:
                result.setdefault("doc_warnings", []).append(warning)
            parsed[cap] = result
        else:
            unparsed.append({"capability": cap, "section": sec["section"], "reason": problem[0], "detail": problem[1]})
    removed = resolve_enum_name_conflicts(parsed)
    for cap, (reason, detail) in removed.items():
        del parsed[cap]
        unparsed.append({"capability": cap, "section": seen[cap], "reason": reason, "detail": detail})
    unparsed.sort(key=lambda u: (u["section"] is None, [int(p) for p in (u["section"] or "0.0").split(".")]))
    return parsed, unparsed, len(sections)


# ── Files, tools, output ──────────────────────────────────────────────────

def sha256_of(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def find_header(sdk):
    for platform in HEADER_GLOB_DIRS:
        path = os.path.join(sdk, "Module", platform, "HeaderFile", "Maid3.h")
        if os.path.isfile(path):
            return path
    raise ExtractError("Maid3.h not found under %s (looked in Module/{%s}/HeaderFile/)" % (sdk, ",".join(HEADER_GLOB_DIRS)))


def find_pdftotext(explicit):
    if explicit:
        if os.path.isfile(explicit) and os.access(explicit, os.X_OK):
            return explicit
        raise ExtractError("--pdftotext %s is not an executable file" % explicit)
    found = shutil.which("pdftotext")
    if found:
        return found
    for path in PDFTOTEXT_FALLBACKS:
        if os.path.isfile(path) and os.access(path, os.X_OK):
            return path
    raise ExtractError(
        "pdftotext (poppler) is not installed, so the SDK's PDF cannot be read.\n"
        "  macOS:  brew install poppler\n"
        "  Debian/Ubuntu:  apt install poppler-utils\n"
        "Then run this again. Without the file, `nikon-fleet show` prints raw numbers.")


def pdf_text(pdftotext, pdf):
    try:
        proc = subprocess.run([pdftotext, "-layout", pdf, "-"], capture_output=True, check=False)
    except OSError as e:
        raise ExtractError("could not run %s: %s" % (pdftotext, e))
    if proc.returncode != 0:
        raise ExtractError("pdftotext failed on %s (exit %d): %s" % (pdf, proc.returncode, proc.stderr.decode("utf-8", "replace").strip()))
    return proc.stdout.decode("utf-8", "replace")


CONFIG_CAPABILITY_RE = re.compile(r"<capability:(kNkMAIDCapability_\w+)-\d+>")


def config_capability_names(path):
    """The `kNkMAIDCapability_X` names MaidLayer.config defines, or None when there is no file."""
    if path is None or not os.path.isfile(path):
        return None
    with open(path, encoding="utf-8", errors="replace") as f:
        return {m.group(1) for line in f for m in CONFIG_CAPABILITY_RE.finditer(line)}


def build(sdk, pdftotext, config=None, now=None):
    if not os.path.isdir(sdk):
        raise ExtractError("%s is not a directory" % sdk)
    header = find_header(sdk)
    pdf = os.path.join(sdk, PDF_RELATIVE)
    if not os.path.isfile(pdf):
        raise ExtractError("%s not found (expected %s inside the SDK folder)" % (pdf, PDF_RELATIVE))
    tool = find_pdftotext(pdftotext)
    with open(header, encoding="utf-8", errors="replace") as f:
        base = base_capabilities(f.read())
    capabilities, unparsed, sections = parse_pdf_text(pdf_text(tool, pdf), config_capability_names(config))
    if sections == 0:
        raise ExtractError("no capability section found in %s: the document layout is not the one this reads" % pdf)
    return {
        "source": {
            "sdk_folder": os.path.basename(os.path.normpath(sdk)),
            "files": [{"path": os.path.relpath(p, sdk), "sha256": sha256_of(p)} for p in (header, pdf)],
            "extractor_version": EXTRACTOR_VERSION,
            "generated_at": (now or datetime.datetime.now(datetime.timezone.utc)).strftime("%Y-%m-%dT%H:%M:%SZ"),
        },
        "base_capabilities": base,
        "capabilities": capabilities,
        "unparsed": unparsed,
        "counts": {"pdf_capability_sections": sections, "with_value_names": len(capabilities), "unparsed": len(unparsed)},
    }


def runtime_dir():
    return os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), os.pardir, "sdk-runtime"))


def main(argv):
    ap = argparse.ArgumentParser(description="Write maid-labels.json from the Nikon Remote SDK documents.")
    ap.add_argument("sdk", help="the SDK folder (the one containing Module/)")
    ap.add_argument("--out", default=None, help="output file (default: sdk-runtime/maid-labels.json of this repository)")
    ap.add_argument("--maid-layer-config", default=None,
                    help="MaidLayer.config, used to settle which name is right where the PDF's heading and "
                         "Capability line disagree (default: sdk-runtime/MaidLayer.config when it exists)")
    ap.add_argument("--pdftotext", default=None, help="path of poppler's pdftotext (default: PATH, then the usual install places)")
    args = ap.parse_args(argv)
    out = os.path.normpath(args.out or os.path.join(runtime_dir(), "maid-labels.json"))
    config = args.maid_layer_config or os.path.join(runtime_dir(), "MaidLayer.config")
    try:
        result = build(args.sdk, args.pdftotext, config)
        os.makedirs(os.path.dirname(out), exist_ok=True)
        tmp = out + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(result, f, indent=1, ensure_ascii=False)
            f.write("\n")
        os.replace(tmp, out)
    except ExtractError as e:
        print("extract-maid-labels: %s" % e, file=sys.stderr)
        return 2
    except OSError as e:
        print("extract-maid-labels: cannot write %s: %s" % (out, e), file=sys.stderr)
        return 2
    c = result["counts"]
    print("wrote %s: %d base capabilities, %d of %d documented capabilities have value names, %d unparsed"
          % (out, len(result["base_capabilities"]), c["with_value_names"], c["pdf_capability_sections"], c["unparsed"]))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
