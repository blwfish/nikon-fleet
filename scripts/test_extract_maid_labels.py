"""Tests for extract-maid-labels.py: python3 -m unittest scripts/test_extract_maid_labels.py

The header and the PDF text below are invented (shaped like what the SDK ships, containing none of
it): the tests do not need the SDK, and a real run is the maintainer's own check.
"""
import importlib.util
import json
import os
import stat
import sys
import tempfile
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("extract_maid_labels", os.path.join(HERE, "extract-maid-labels.py"))
x = importlib.util.module_from_spec(_spec)
sys.modules["extract_maid_labels"] = x
_spec.loader.exec_module(x)

HEADER = """
enum eNkMAIDCapability
{
    kNkMAIDCapability_Alpha = 1,    // first
    kNkMAIDCapability_Beta,    // second
    kNkMAIDCapability_Gamma,

    /* a block comment, kNkMAIDCapability_Bogus = 99, */
    kNkMAIDCapability_Delta,
    kNkMAIDCapability_Epsilon,
\tkNkMAIDCapability_Zeta = kNkMAIDCapability_Epsilon + 2,    // a jump
    kNkMAIDCapability_Eta,
    kNkMAIDCapability_Wide = (kNkMAIDCapability_Eta << 2) | 1,
    kNkMAIDCapability_VendorBase = 0x8000    // vendor supplied capabilities start here
};
"""


def section(number, title, cap=None, types=("kNkMAIDCapType_Enum", "kNkMAIDCapType_Unsigned"), data=()):
    """A capability section as `pdftotext -layout` lays it out (invented text)."""
    out = ["", "%s %s" % (number, title), "  This sets something.", "  Capability        kNkMAIDCapability_%s" % (cap or title), "  Object types      Source"]
    out.append("  ulType            %s" % types[0])
    out.extend("                    %s" % t for t in types[1:])
    out.append("  ulOperations      kNkMAIDCapOperation_Get, kNkMAIDCapOperation_Set")
    out.append("")
    out.extend(data)
    out.extend(["", "  Result Code", "     NKERROR     One of the NKERROR value we received"])
    return out


def enum_data(name, entries, lead="Data             One of %s"):
    lines = ["  " + lead % name]
    lines.extend("                    %s" % e for e in entries)
    return lines


def parse(*sections, known=None):
    text = "\n".join(l for s in sections for l in s) + "\n"
    return x.parse_pdf_text(text, known)


def by_cap(unparsed):
    return {u["capability"]: u["reason"] for u in unparsed}


class CEnum(unittest.TestCase):
    def test_values_are_computed_as_a_c_compiler_would(self):
        names = dict(x.parse_c_enum(HEADER, "eNkMAIDCapability"))
        self.assertEqual(names["kNkMAIDCapability_Alpha"], 1)
        self.assertEqual(names["kNkMAIDCapability_Gamma"], 3)
        self.assertEqual(names["kNkMAIDCapability_Epsilon"], 5)
        self.assertEqual(names["kNkMAIDCapability_Zeta"], 7, "Epsilon + 2, not Epsilon + 1")
        self.assertEqual(names["kNkMAIDCapability_Eta"], 8, "counting resumes after the jump")
        self.assertEqual(names["kNkMAIDCapability_Wide"], (8 << 2) | 1)
        self.assertNotIn("kNkMAIDCapability_Bogus", names, "a name in a comment is not an enumerator")

    def test_base_capabilities_are_by_number_without_the_vendor_marker(self):
        base = x.base_capabilities(HEADER)
        self.assertEqual(base["1"], "Alpha")
        self.assertEqual(base["7"], "Zeta")
        self.assertNotIn("6", base, "the number the jump skipped stays a gap")
        self.assertFalse(any(v == "VendorBase" for v in base.values()))
        self.assertNotIn(str(0x8000), base)

    def test_expressions(self):
        ev = lambda e, names=None: x.eval_c_expr(e, names or {})  # noqa: E731
        self.assertEqual(ev("0x10"), 16)
        self.assertEqual(ev("0X1f"), 31)
        self.assertEqual(ev("1 + 2 * 3"), 7)
        self.assertEqual(ev("(1 + 2) * 3"), 9)
        self.assertEqual(ev("1 << 4 | 1"), 17)
        self.assertEqual(ev("1 << 1 + 1"), 4, "a shift binds looser than an addition")
        self.assertEqual(ev("1 | 2 << 1"), 5, "and tighter than an or")
        self.assertEqual(ev("-3 + 5"), 2)
        self.assertEqual(ev("A + 2", {"A": 5}), 7)

    def test_what_cannot_be_evaluated_is_an_error_never_a_guess(self):
        for bad in ["B + 1", "1 +", "(1", "1 2", "3 / 1", "1 $ 2", ""]:
            with self.assertRaises(x.ExtractError, msg=bad):
                x.eval_c_expr(bad, {})

    def test_enum_errors(self):
        with self.assertRaises(x.ExtractError):
            x.parse_c_enum("enum other { a };", "eNkMAIDCapability")
        with self.assertRaises(x.ExtractError):
            x.parse_c_enum("enum e { a = 1,\n#if X\n b };", "e")
        with self.assertRaises(x.ExtractError):
            x.parse_c_enum("enum e { a, a };", "e")
        with self.assertRaises(x.ExtractError):
            x.base_capabilities("enum eNkMAIDCapability { kNkMAIDCapability_A = 1, kNkMAIDCapability_B = 1 };")
        with self.assertRaises(x.ExtractError):
            x.base_capabilities("enum eNkMAIDCapability { Wrong_A = 1 };")


class PdfSections(unittest.TestCase):
    def test_an_enumeration_gets_its_values_and_where_they_came_from(self):
        caps, unparsed, n = parse(section("3.10", "ModeOne", data=enum_data("eNkMAIDModeOne", ["0: Name zero", "1: Name one", "15: Name fifteen(A1)"])))
        self.assertEqual((n, unparsed), (1, []))
        self.assertEqual(caps["ModeOne"], {
            "enum": "eNkMAIDModeOne",
            "values": {"0": "Name zero", "1": "Name one", "15": "Name fifteen(A1)"},
            "provenance": "MAID3-COMMON(E).pdf section 3.10",
        })

    def test_hex_values_are_read_as_hex_and_lowercase_one_of_is_accepted(self):
        caps, _, _ = parse(section("3.3", "Resp", types=("kNkMAIDCapType_Unsigned",), data=enum_data("eResp", ["0x10: Ended", "0x11: Out of focus"], lead="Data             one of %s")))
        self.assertEqual(caps["Resp"]["values"], {"16": "Ended", "17": "Out of focus"})

    def test_the_typo_one_enum_and_one_of_the_value_are_accepted_when_a_list_follows(self):
        caps, _, _ = parse(section("3.1", "A", types=("kNkMAIDCapType_Unsigned",), data=enum_data("eNkMAIDA", ["0: x", "1: y"], lead="Data             one %s")))
        self.assertEqual(caps["A"]["enum"], "eNkMAIDA")
        caps, _, _ = parse(section("3.2", "B", data=enum_data("eNkMAIDB", ["0: x"], lead="Data   One of the %s value")))
        self.assertEqual(caps["B"]["enum"], "eNkMAIDB")

    def test_only_enum_like_types_are_read(self):
        for types in [("kNkMAIDCapType_Range",), ("kNkMAIDCapType_Boolean",), ("kNkMAIDCapType_String",)]:
            caps, unparsed, _ = parse(section("3.1", "Z", types=types, data=enum_data("eZ", ["0: a", "1: b"])))
            self.assertEqual((caps, by_cap(unparsed)), ({}, {"Z": x.NOT_ENUM_TYPE}), types)
        for types in [("kNkMAIDCapType_Enum", "kNkMAIDArrayType_PackedString"), ("kNkMAIDCapType_Enum", "kNkMAIDArrayType_Unsigned"), ("kNkMAIDCapType_Unsigned",)]:
            caps, _, _ = parse(section("3.1", "Z", types=types, data=enum_data("eZ", ["0: a", "1: b"])))
            self.assertIn("Z", caps, types)

    def test_a_type_written_with_a_stray_space_is_still_read(self):
        text = section("3.1", "Z", data=enum_data("eZ", ["0: a", "1: b"]))
        text = [l.replace("kNkMAIDCapType_Enum", "kNkMAIDCapType_ Enum") for l in text]
        caps, _, _ = x.parse_pdf_text("\n".join(text), None)
        self.assertIn("Z", caps)

    def test_leaving_a_capability_unparsed_has_a_reason_for_each_case(self):
        cases = {
            "NoStatement": (["  Data             Strings of F value"], x.NO_ENUM_STATEMENT),
            "NoList": (["  Data             one of eX", "  For valid values, see the property."], x.NO_NUMBERED_LIST),
            "NamesOnly": (enum_data("eX", ["eX_A,", "eX_B,"]), x.NO_NUMBERED_LIST),
            "Twice": (enum_data("eX", ["0: a", "0: b"]), x.DUPLICATE_VALUE),
            "Big": (enum_data("eX", ["0: a", "4294967296: b"]), x.VALUE_OUT_OF_RANGE),
            "Flags": (enum_data("eX", ["1: a", "2: b", "4: c"]), x.BIT_FLAGS),
            "Wrapped": (enum_data("eX", ["0: a", "1: a long name that", "      continues here"]), x.LIST_MAY_BE_INCOMPLETE),
            "Resumed": (enum_data("eX", ["0: a", "1: b"]) + ["  Some other text.", "                    2: c"], x.LIST_MAY_BE_INCOMPLETE),
            "Two": (["  Data  one of eA", "                    0: a", "  and also one of eB", "                    0: b"], x.AMBIGUOUS_ENUM_NAME),
        }
        for cap, (data, reason) in cases.items():
            caps, unparsed, _ = parse(section("3.1", cap, data=data))
            self.assertEqual((caps, by_cap(unparsed)), ({}, {cap: reason}), cap)

    def test_a_value_is_32_bits_at_below_and_above_the_limit(self):
        for value, ok in [("4294967294", True), ("4294967295", True), ("4294967296", False), ("0xFFFFFFFF", True), ("0x100000000", False)]:
            caps, unparsed, _ = parse(section("3.1", "V", data=enum_data("eV", ["0: a", "%s: b" % value])))
            self.assertEqual("V" in caps, ok, value)
            if not ok:
                self.assertEqual(by_cap(unparsed), {"V": x.VALUE_OUT_OF_RANGE})
        caps, _, _ = parse(section("3.1", "V", data=enum_data("eV", ["010: ten", "0x10: sixteen", "9: nine"])))
        self.assertEqual(caps["V"]["values"], {"10": "ten", "16": "sixteen", "9": "nine"}, "a leading zero is not octal")

    def test_bit_flags_need_three_single_bit_values_at_the_boundary(self):
        for entries, flags in [(["1: a", "2: b"], False), (["1: a", "2: b", "4: c"], True), (["1: a", "2: b", "3: c"], False), (["0: a", "1: b", "2: c"], False), (["16: a", "256: b", "4096: c"], True)]:
            caps, unparsed, _ = parse(section("3.1", "F", data=enum_data("eF", entries)))
            self.assertEqual("F" not in caps, flags, entries)
            if flags:
                self.assertEqual(by_cap(unparsed), {"F": x.BIT_FLAGS})

    def test_a_line_after_the_list_indented_less_than_it_ends_the_list_and_like_it_may_continue_it(self):
        less = enum_data("eX", ["0: a", "1: b"]) + ["  The following cases, this is read only."]
        caps, _, _ = parse(section("3.1", "L", data=less))
        self.assertEqual(list(caps["L"]["values"]), ["0", "1"])
        like = enum_data("eX", ["0: a", "1: b"]) + ["                    4. c"]
        caps, unparsed, _ = parse(section("3.1", "L", data=like))
        self.assertEqual((caps, by_cap(unparsed)), ({}, {"L": x.LIST_MAY_BE_INCOMPLETE}), "`4. c` is a typo for `4: c`, and is not guessed")
        shallower_by_one = enum_data("eX", ["0: a", "1: b"]) + ["                   continues"]
        self.assertEqual(parse(section("3.1", "L", data=shallower_by_one))[0]["L"]["values"], {"0": "a", "1": "b"})
        same = enum_data("eX", ["0: a", "1: b"]) + ["                    continues"]
        self.assertEqual(by_cap(parse(section("3.1", "L", data=same))[1]), {"L": x.LIST_MAY_BE_INCOMPLETE})

    def test_the_text_after_the_list_that_disagrees_with_it_is_a_warning_not_a_correction(self):
        data = enum_data("eX", ["15: Fifteen(A1)", "16: Sixteen(A2)", "18: Eighteen(A3)"]) + [
            "  If set to “15: Fifteen(A 1)”, “16: Sixteen (A2)” or “17: Seventeen(A3)”, the mode is used.",
            "  The ratio “16:9” is not a constant.",
        ]
        caps, _, _ = parse(section("3.1", "W", data=data))
        self.assertEqual(caps["W"]["values"]["18"], "Eighteen(A3)", "the list stands as written")
        self.assertEqual(caps["W"]["doc_warnings"], ["the text names 17: Seventeen(A3), which the list does not have"], "spacing is not a disagreement; a ratio is not a constant")
        data = enum_data("eX", ["0: Off", "1: On"]) + ["  “1: Low” is the other."]
        self.assertIn("the list has 1: On", parse(section("3.1", "W", data=data))[0]["W"]["doc_warnings"][0])

    def test_a_form_feed_between_pages_does_not_hide_a_section(self):
        text = "\n".join(section("3.1", "P", data=enum_data("eP", ["0: a", "1: b"])))
        text = text.replace("  Capability", "\f  Capability")
        self.assertIn("P", x.parse_pdf_text(text, None)[0])

    def test_a_section_that_is_not_a_capability_is_not_one(self):
        text = "\n".join(["2.1 Something", "  Capability is a word here.", "  more"])
        self.assertEqual(x.parse_pdf_text(text, None), ({}, [], 0))


class Names(unittest.TestCase):
    def test_a_heading_that_disagrees_with_the_capability_line_is_settled_only_by_the_config(self):
        sec = section("3.17", "HeadingName", cap="c", data=enum_data("eL", ["1: One", "2: Two"]))
        caps, unparsed, _ = parse(sec)
        self.assertEqual((caps, by_cap(unparsed)), ({}, {"c": x.NAME_MISMATCH}), "no config: not settled")
        caps, _, _ = parse(sec, known={"kNkMAIDCapability_HeadingName"})
        self.assertIn("HeadingName", caps)
        self.assertIn("MaidLayer.config defines only HeadingName", caps["HeadingName"]["doc_warnings"][0])
        caps, _, _ = parse(sec, known={"kNkMAIDCapability_c"})
        self.assertIn("c", caps, "the Capability line wins when the config defines only it")
        for known in [set(), {"kNkMAIDCapability_c", "kNkMAIDCapability_HeadingName"}]:
            caps, unparsed, _ = parse(sec, known=known)
            self.assertEqual((caps, by_cap(unparsed)), ({}, {"c": x.NAME_MISMATCH}), "neither or both known: not settled %s" % known)

    def test_agreeing_names_need_no_config(self):
        caps, _, _ = parse(section("3.1", "Same", data=enum_data("eS", ["0: a"])), known=set())
        self.assertIn("Same", caps)

    def test_two_sections_for_one_capability_leave_it_unparsed(self):
        a = section("3.1", "Dup", data=enum_data("eD", ["0: a", "1: b"]))
        b = section("3.2", "Dup", data=enum_data("eD", ["0: a", "1: b"]))
        caps, unparsed, _ = parse(a, b)
        self.assertEqual((caps, by_cap(unparsed)), ({}, {"Dup": x.DUPLICATE_SECTION}))


class EnumNameConflicts(unittest.TestCase):
    def test_an_enum_name_given_different_constants_keeps_only_its_own_capability(self):
        own = section("3.98", "SpaceOwner", data=enum_data("eNkMAIDSpaceOwner", ["0: Red", "1: Green", "100: Blue"]))
        copy = section("3.103", "Copier", data=enum_data("eNkMAIDSpaceOwner", ["0: Left", "1: Right", "2: Up"]))
        caps, unparsed, _ = parse(own, copy)
        self.assertEqual(list(caps), ["SpaceOwner"])
        self.assertEqual(by_cap(unparsed), {"Copier": x.ENUM_NAME_CONFLICT})
        self.assertIn("SpaceOwner is its own capability", unparsed[0]["detail"])

    def test_with_no_capability_owning_the_name_all_the_conflicting_ones_go(self):
        a = section("3.1", "Alpha", data=enum_data("eShared", ["0: x", "1: y"]))
        b = section("3.2", "Beta", data=enum_data("eShared", ["0: p", "1: q"]))
        caps, unparsed, _ = parse(a, b)
        self.assertEqual((caps, by_cap(unparsed)), ({}, {"Alpha": x.ENUM_NAME_CONFLICT, "Beta": x.ENUM_NAME_CONFLICT}))

    def test_one_enum_documented_as_a_subset_by_two_capabilities_is_not_a_conflict(self):
        a = section("3.54", "ToneA", data=enum_data("eNkMAIDToneA", ["0: Flat", "2: Steep"]))
        b = section("3.70", "MovieToneA", data=enum_data("eNkMAIDToneA", ["0: Flat", "1: Log", "2: Steep"]))
        caps, unparsed, _ = parse(a, b)
        self.assertEqual((sorted(caps), unparsed), (["MovieToneA", "ToneA"], []))

    def test_a_section_that_does_not_conflict_with_the_owner_survives_in_a_conflicted_group(self):
        own = section("3.1", "Mode", data=enum_data("eMode", ["0: a", "1: b"]))
        bad = section("3.2", "Other", data=enum_data("eMode", ["0: x"]))
        fine = section("3.3", "Third", data=enum_data("eMode", ["5: z"]))
        caps, unparsed, _ = parse(own, bad, fine)
        self.assertEqual((sorted(caps), by_cap(unparsed)), (["Mode", "Third"], {"Other": x.ENUM_NAME_CONFLICT}))


class EndToEnd(unittest.TestCase):
    def sdk(self, tmp, pdf_text):
        sdk = os.path.join(tmp, "S-SDK-TEST")
        os.makedirs(os.path.join(sdk, "Module", "Mac", "HeaderFile"))
        os.makedirs(os.path.join(sdk, "Module", "Document", "English"))
        with open(os.path.join(sdk, "Module", "Mac", "HeaderFile", "Maid3.h"), "w") as f:
            f.write(HEADER)
        with open(os.path.join(sdk, x.PDF_RELATIVE), "wb") as f:
            f.write(b"%PDF-not really")
        fake = os.path.join(tmp, "fake-pdftotext")
        with open(fake, "w") as f:
            f.write("#!/bin/sh\ncat <<'EOT'\n%s\nEOT\n" % pdf_text)
        os.chmod(fake, os.stat(fake).st_mode | stat.S_IXUSR)
        return sdk, fake

    def text(self):
        return "\n".join(section("3.10", "ModeOne", data=enum_data("eNkMAIDModeOne", ["0: Name zero", "3: Name three"]))
                         + section("3.1", "TextOne", types=("kNkMAIDCapType_Enum", "kNkMAIDArrayType_PackedString"), data=["  Data   Strings of text"]))

    def test_the_file_has_the_documented_shape(self):
        with tempfile.TemporaryDirectory() as tmp:
            sdk, fake = self.sdk(tmp, self.text())
            out = os.path.join(tmp, "out", "maid-labels.json")
            self.assertEqual(x.main([sdk, "--out", out, "--pdftotext", fake]), 0)
            with open(out) as f:
                d = json.load(f)
        self.assertEqual(d["source"]["sdk_folder"], "S-SDK-TEST")
        self.assertEqual(d["source"]["extractor_version"], x.EXTRACTOR_VERSION)
        self.assertEqual([f["path"] for f in d["source"]["files"]], [os.path.join("Module", "Mac", "HeaderFile", "Maid3.h"), x.PDF_RELATIVE])
        self.assertTrue(all(len(f["sha256"]) == 64 for f in d["source"]["files"]))
        self.assertRegex(d["source"]["generated_at"], r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$")
        self.assertEqual(d["base_capabilities"]["1"], "Alpha")
        self.assertEqual(d["capabilities"]["ModeOne"]["values"], {"0": "Name zero", "3": "Name three"})
        self.assertEqual(d["counts"], {"pdf_capability_sections": 2, "with_value_names": 1, "unparsed": 1})
        self.assertEqual([(u["capability"], u["reason"]) for u in d["unparsed"]], [("TextOne", x.NO_ENUM_STATEMENT)])
        self.assertTrue(all(u["reason"] in x.REASONS for u in d["unparsed"]))

    def test_a_missing_tool_or_file_is_a_clear_message_and_status_2(self):
        with tempfile.TemporaryDirectory() as tmp:
            sdk, fake = self.sdk(tmp, self.text())
            out = os.path.join(tmp, "o.json")
            with mock.patch("sys.stderr") as err:
                self.assertEqual(x.main([sdk, "--out", out, "--pdftotext", os.path.join(tmp, "nowhere")]), 2)
                self.assertEqual(x.main([os.path.join(tmp, "no-sdk"), "--out", out]), 2)
                os.remove(os.path.join(sdk, x.PDF_RELATIVE))
                self.assertEqual(x.main([sdk, "--out", out, "--pdftotext", fake]), 2)
            self.assertFalse(os.path.exists(out), "nothing is written on a failure")
            self.assertTrue(err.write.called)

    def test_no_pdftotext_anywhere_says_how_to_get_it(self):
        with mock.patch.object(x.shutil, "which", return_value=None), mock.patch.object(x, "PDFTOTEXT_FALLBACKS", ()):
            with self.assertRaises(x.ExtractError) as cm:
                x.find_pdftotext(None)
        self.assertIn("brew install poppler", str(cm.exception))
        self.assertIn("poppler-utils", str(cm.exception))

    def test_a_pdf_with_no_capability_sections_is_refused_not_written_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            sdk, fake = self.sdk(tmp, "nothing like the manual")
            out = os.path.join(tmp, "o.json")
            self.assertEqual(x.main([sdk, "--out", out, "--pdftotext", fake]), 2)
            self.assertFalse(os.path.exists(out))

    def test_pdftotext_failing_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            sdk, _ = self.sdk(tmp, "")
            bad = os.path.join(tmp, "bad-pdftotext")
            with open(bad, "w") as f:
                f.write("#!/bin/sh\necho oops >&2\nexit 3\n")
            os.chmod(bad, 0o755)
            with self.assertRaises(x.ExtractError) as cm:
                x.build(sdk, bad)
        self.assertIn("exit 3", str(cm.exception))
        self.assertIn("oops", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
