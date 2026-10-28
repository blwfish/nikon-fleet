import json
import subprocess
from pathlib import Path
from typing import ClassVar

import pytest

from conftest import FIXTURES, PROJECT
from fleet_lib import (
    EXIT_ALREADY_ARCHIVED, EXIT_WRITE_INDETERMINATE, FTP_PASSWORD_KEY, ftp_write_failure_text, is_id_fallback, reference_file_name, FTP_TEXT_FIELDS, CliOutputError, accept_zip_entry, browse_outcome, camera_key, camera_label,
    firmware_add_argv, firmware_browse_argv, firmware_meta_problem, firmware_display_rows, firmware_ls_argv, flag, fmt_cap_value,
    ftp_form_values, global_argv, is_plain_name_component, is_valid_u16, join_output, known_cameras,
    listing_notes, listing_ref_marks, ls_argv, model_slug, parse_discover, parse_firmware_listing,
    parse_fw_filename, parse_json_stdout, parse_listing, parse_port, parse_propcode, parse_settings_show,
    property_rows, read_snapshot_file, ref_set_argv, rows_for_camera, set_data_dir_argv, settings_show_argv,
    snapshot_argv, snapshot_bytes_problem, snapshot_problem, suggest_fw_model, vendor_read_argv, vendor_write_ftp_argv,
)

_FIXTURE = PROJECT / "tests" / "fixtures" / "capability_value_shapes.json"
_VALIDITY = PROJECT / "tests" / "fixtures" / "snapshot_validity"
_REFERENCE_PARITY = PROJECT / "tests" / "fixtures" / "reference_parity"


def _read(name: str) -> str:
    return (FIXTURES / name).read_text(encoding="utf-8")


# ── model_slug ─────────────────────────────────────────────────────────────────
# Parity with the Rust side's firmware::model_slug() — both must produce
# identical filenames from the same model string, or reference/archive
# lookups silently fail to find each other's files.

class TestModelSlug:
    def test_replaces_spaces(self):
        assert model_slug("Z 9") == "Z_9"
        assert model_slug("Z 6III") == "Z_6III"

    def test_replaces_every_space_not_just_the_first(self):
        assert model_slug("Z 6 III") == "Z_6_III"
        assert model_slug("  ") == "__"

    def test_no_spaces_unchanged(self):
        assert model_slug("Z6_3") == "Z6_3"


# ── is_plain_name_component ───────────────────────────────────────────────────

# Copied from `PLAIN_COMPONENT_TABLE` in src/ops/paths.rs, the table every Rust
# caller of `is_safe_path_component` runs. Keep the two lists identical.
PLAIN_COMPONENT_TABLE = [
    ("5.31", True), ("Z_9", True), ("Z 9", True), ("a.json", True), ("...", True), (".hidden", True),
    ("id-1", True), ("é", True), ("0000003023668", True), ("a\nb", True), ("a ", True),
    ("", False), (".", False), ("..", False), ("../../../../tmp/pwned", False), ("../x", False),
    ("./a", False), ("a/b", False), ("a/", False), ("/", False), ("/etc/passwd", False),
    ("a\\b", False), ("\\", False), ("a\0b", False), ("\0", False),
]


class TestIsPlainNameComponent:
    @pytest.mark.parametrize("value,expected", PLAIN_COMPONENT_TABLE)
    def test_matches_the_rust_table(self, value, expected):
        assert is_plain_name_component(value) is expected

    def test_non_strings_are_not_plain(self):
        assert not is_plain_name_component(None)
        assert not is_plain_name_component(5)


# ── parse_json_stdout ─────────────────────────────────────────────────────────

class TestParseJsonStdout:
    def test_a_single_document(self):
        assert parse_json_stdout('{"cameras": []}\n') == {"cameras": []}

    def test_banner_before_the_json_is_an_error_not_skipped(self):
        # The CLI keeps SDK banner text off stdout in --json modes; if it ever
        # leaks, say so instead of digging a `{` out of the middle.
        with pytest.raises(CliOutputError) as e:
            parse_json_stdout('InitializeSDK Execution duration: 204\n{"cameras":[]}', "discover --json output")
        assert "discover --json output" in str(e.value)
        assert "InitializeSDK" in str(e.value)

    def test_empty_is_an_error(self):
        with pytest.raises(CliOutputError):
            parse_json_stdout("")

    def test_two_documents_are_an_error(self):
        with pytest.raises(CliOutputError):
            parse_json_stdout('{"a":1}\n{"b":2}')


# ── accept_zip_entry ──────────────────────────────────────────────────────────

_ZIP_NAMES = FIXTURES.parent / "tests-fixtures" / "zip_entry_names.json"


class TestAcceptZipEntry:
    """The cases live in gui/tests-fixtures/zip_entry_names.json, which the Rust
    GUI's `classify_zip_entry` runs too; it is the definition, this reads it."""

    DOC = json.loads(_ZIP_NAMES.read_text(encoding="utf-8"))
    CASES = DOC["cases"]

    def test_the_fixture_has_both_verdicts(self):
        assert any(c["accept"] for c in self.CASES) and any(not c["accept"] for c in self.CASES)
        assert any(c["name"] == "firmware/Z_9/5.31/source-page.html" and c["accept"] for c in self.CASES)

    @pytest.mark.parametrize("case", CASES, ids=lambda c: repr(c["name"]))
    def test_matches_the_fixture(self, case):
        assert accept_zip_entry(case["name"]) is case["accept"], case["why"]

    # Cases the shared table does not name, each of which a mutation of the rule survived.
    @pytest.mark.parametrize("name,accepted", [
        ("snapshots//a.json", False),                       # empty component: not collapsed
        ("firmware/Z_9//5.31/firmware.bin", False),
        ("snapshots/a.json/", False),
        ("./snapshots/a.json", False),
        ("snapshots/./a.json", False),
        ("firmware/Z_9/./firmware.bin", False),
        ("snapshots/a\\b.json", False),
        ("snapshots/a\0.json", False),
        ("firmware/Z_9/5.31/extra/firmware.bin", False),
        ("firmware/Z_9/firmware.bin", False),
        ("snapshots/sub/a.json", False),
        ("firmware//5.31/firmware.bin", False),            # empty model component, still four parts
        ("firmware/Z_9//firmware.bin", False),             # empty version component, still four parts
        ("firmware/Z_9/5.31/firmware.bin/x", False),       # a file name used as a directory
        ("firmware/Z_9/5.31/firmware.bin/", False),        # trailing slash on a valid entry
    ])
    def test_extra_cases(self, name, accepted):
        assert accept_zip_entry(name) is accepted


# ── parse_fw_filename ─────────────────────────────────────────────────────────

class TestParseFwFilename:
    # real Nikon firmware file names
    @pytest.mark.parametrize("name,expected", [
        ("Z_9_0531.bin", ("Z_9", "5.31")),
        ("Z_9_0532.bin", ("Z_9", "5.32")),
        ("Z6_3_0200.bin", ("Z6_3", "2.00")),
        ("Z6_2_0170.bin", ("Z6_2", "1.70")),
        ("Z_30_0120.bin", ("Z_30", "1.20")),
        ("Z_8_0310.bin", ("Z_8", "3.10")),
        ("Z_9_0100.bin", ("Z_9", "1.00")),       # major zero-padded
        ("Z_9_1020.bin", ("Z_9", "10.20")),      # two-digit major
    ])
    def test_known_files(self, name, expected):
        assert parse_fw_filename(name) == expected

    @pytest.mark.parametrize("name", [
        "Z_9_v531.bin", "Z_9_531.bin", "Z_9_05310.bin", "firmware.bin",
        "Z_9_05²³.bin",      # superscripts: str.isdigit() is True for them, int() raises
        "Z_9_٥٣١٠.bin",   # Arabic-Indic digits: int() accepts them, they are not ASCII
        "Z_9_０５３１.bin",   # full-width digits
    ])
    def test_non_ascii_digit_or_wrong_shape_has_no_version(self, name):
        stem, version = parse_fw_filename(name)
        assert version == ""
        assert stem == Path(name).stem

    def test_extension_not_included_in_model(self):
        stem, _ = parse_fw_filename("Z_9_0531.bin")
        assert not stem.endswith(".bin")


class TestSuggestFwModel:
    # `firmware ls --model Z6_3` and the rollback model check match the stored
    # model, so "Z6_3_0200.bin" must not become "Z6 3".
    def test_z6iii_keeps_its_underscore_when_the_model_is_known(self):
        stem, _ = parse_fw_filename("Z6_3_0200.bin")
        assert suggest_fw_model(stem, {"Z 9", "Z6_3"}) == "Z6_3"

    def test_z9_gets_its_space_when_the_model_is_known(self):
        stem, _ = parse_fw_filename("Z_9_0531.bin")
        assert suggest_fw_model(stem, {"Z 9", "Z6_3"}) == "Z 9"

    def test_an_unknown_model_is_not_guessed(self):
        stem, _ = parse_fw_filename("Z_30_0120.bin")
        assert suggest_fw_model(stem, {"Z 9"}) == "Z_30"
        assert suggest_fw_model("Z6_3", set()) == "Z6_3"

    def test_the_suggestion_has_the_files_slug(self):
        for name, known in (("Z6_3_0200.bin", {"Z6_3"}), ("Z_9_0531.bin", {"Z 9"})):
            stem, _ = parse_fw_filename(name)
            assert model_slug(suggest_fw_model(stem, known)) == stem

    def test_two_known_spellings_pick_the_spaced_one_deterministically(self):
        assert suggest_fw_model("Z_9", {"Z_9", "Z 9"}) == "Z 9"


# ── parse_port ────────────────────────────────────────────────────────────────

class TestParsePort:
    def test_at_below_above_the_lower_bound(self):
        with pytest.raises(ValueError):
            parse_port("0")
        assert parse_port("1") == 1
        assert parse_port("2") == 2

    def test_at_below_above_the_upper_bound(self):
        assert parse_port("65534") == 65534
        assert parse_port("65535") == 65535
        with pytest.raises(ValueError):
            parse_port("65536")

    def test_default_ftp_port(self):
        assert parse_port("21") == 21

    @pytest.mark.parametrize("text", [
        "2_1", "+21", "-21", "", " ", "21.0", "0x15", "٢١", "²", "２１", "2 1", "ftp",
    ])
    def test_anything_but_ascii_digits_is_refused(self, text):
        with pytest.raises(ValueError):
            parse_port(text)

    def test_surrounding_whitespace_is_ignored(self):
        assert parse_port("  21\n") == 21

    def test_leading_zeros_are_digits(self):
        assert parse_port("0021") == 21


# ── parse_propcode ────────────────────────────────────────────────────────────
# Must accept the same forms as the Rust CLI's own parse_propcode (main.rs) —
# both parse the same user-typed string, one from a GUI entry, one from argv.

class TestIsValidU16:
    def test_zero_is_valid(self):
        assert is_valid_u16(0)

    def test_max_is_valid(self):
        assert is_valid_u16(0xFFFF)

    def test_one_over_max_is_invalid(self):
        assert not is_valid_u16(0x10000)

    def test_negative_is_invalid(self):
        assert not is_valid_u16(-1)


class TestParsePropcode:
    def test_lowercase_hex(self):
        assert parse_propcode("0xd053") == 0xD053

    def test_uppercase_hex_prefix_and_digits(self):
        assert parse_propcode("0XD053") == 0xD053

    def test_decimal(self):
        assert parse_propcode("53331") == 53331

    def test_strips_whitespace(self):
        assert parse_propcode("  0xD053  ") == 0xD053

    def test_zero(self):
        assert parse_propcode("0x0") == 0

    def test_max_u16(self):
        assert parse_propcode("0xFFFF") == 0xFFFF

    def test_invalid_hex_raises(self):
        with pytest.raises(ValueError):
            parse_propcode("0xZZZZ")

    def test_invalid_decimal_raises(self):
        with pytest.raises(ValueError):
            parse_propcode("not-a-number")

    def test_out_of_range_raises(self):
        with pytest.raises(ValueError):
            parse_propcode("0x10000")

    def test_negative_raises(self):
        with pytest.raises(ValueError):
            parse_propcode("-1")

    def test_empty_and_bare_prefix_raise(self):
        for text in ("", " ", "0x", "0X", "+", "0x+"):
            with pytest.raises(ValueError):
                parse_propcode(text)

    def test_non_ascii_digits_raise(self):
        # int() accepts these; the Rust parser does not.
        with pytest.raises(ValueError):
            parse_propcode("٥٣")

    # Two confirmed divergences from the Rust CLI's parse_propcode
    # (src/ptp_usb.rs): Python's int() tolerates both forms below.
    def test_internal_whitespace_after_hex_prefix_raises(self):
        with pytest.raises(ValueError):
            parse_propcode("0x 10")

    def test_underscore_digit_separators_raise(self):
        with pytest.raises(ValueError):
            parse_propcode("0xD0_53")
        with pytest.raises(ValueError):
            parse_propcode("53_331")

    def test_leading_plus_accepted(self):
        # Matches Rust: u16::from_str_radix/FromStr both accept a leading '+'.
        assert parse_propcode("+53331") == 53331
        assert parse_propcode("0x+D053") == 0xD053


# ── snapshot validity ─────────────────────────────────────────────────────────

class TestSnapshotValidityFixture:
    """tests/fixtures/snapshot_validity: the shared definition of a readable
    snapshot. The Rust CLI's `ls` and the egui GUI run it too."""

    EXPECTED = json.loads((_VALIDITY / "expected.json").read_text())

    def test_the_fixture_is_not_empty_and_has_all_three_verdicts(self):
        assert set(self.EXPECTED.values()) == {"ok", "json", "unsupported_version"}

    def test_every_file_in_the_directory_has_an_expectation(self):
        on_disk = {p.name for p in (_VALIDITY / "snapshots").glob("*.json")}
        assert on_disk == set(self.EXPECTED)

    @pytest.mark.parametrize("name,verdict", sorted(EXPECTED.items()))
    def test_verdict_per_file(self, name, verdict):
        doc, problem = read_snapshot_file(_VALIDITY / "snapshots" / name)
        if verdict == "ok":
            assert problem is None and isinstance(doc, dict)
        else:
            assert doc is None and problem[0] == verdict, problem

    def test_agrees_with_what_the_cli_reported_for_the_same_directory(self):
        # gui/fixtures/ls_snapshot_validity.json is `ls --json` over that directory.
        listing = parse_listing(_read("ls_snapshot_validity.json"))
        listed = {r.file: "ok" for r in listing.rows} | {u.file: u.error_kind for u in listing.unreadable}
        assert listed == self.EXPECTED

    def test_a_missing_file_is_an_io_problem_not_an_exception(self):
        doc, problem = read_snapshot_file(_VALIDITY / "snapshots" / "nope.json")
        assert doc is None and problem[0] == "io"


def _snap(**over):
    doc = {"format_version": 1, "camera": {"model": "Z 9", "serial": "S", "firmware": "5.31"},
           "captured_at": "2026-01-01T00:00:00Z", "label": None, "transport": "usb",
           "properties": {"kNkMAIDCapability_A": {"code": 1, "value": 1}}}
    doc.update(over)
    return doc


class TestSnapshotProblem:
    def test_valid(self):
        assert snapshot_problem(_snap()) is None

    def test_label_may_be_absent_null_or_a_string(self):
        doc = _snap()
        del doc["label"]
        assert snapshot_problem(doc) is None
        assert snapshot_problem(_snap(label="x")) is None
        assert snapshot_problem(_snap(label=5))[0] == "json"

    def test_a_null_property_value_is_a_value(self):
        assert snapshot_problem(_snap(properties={"a": {"code": 1, "value": None}})) is None

    def test_format_version_boundaries(self):
        assert snapshot_problem(_snap(format_version=1)) is None
        assert snapshot_problem(_snap(format_version=0))[0] == "unsupported_version"
        assert snapshot_problem(_snap(format_version=2))[0] == "unsupported_version"
        assert snapshot_problem(_snap(format_version=0xFFFFFFFF))[0] == "unsupported_version"
        assert snapshot_problem(_snap(format_version=0x100000000))[0] == "json"   # not a u32
        assert snapshot_problem(_snap(format_version=-1))[0] == "json"

    @pytest.mark.parametrize("version", [True, 1.0, "1", None, [1]])
    def test_format_version_must_be_an_integer(self, version):
        # `True == 1` and `1.0 == 1` in Python; serde's u32 refuses both.
        assert snapshot_problem(_snap(format_version=version))[0] == "json"

    def test_shape_errors_are_reported_before_the_version(self):
        assert snapshot_problem(_snap(format_version=999, transport="pigeon"))[0] == "json"

    @pytest.mark.parametrize("transport", ["usb", "ptpip", "file"])
    def test_known_transports(self, transport):
        assert snapshot_problem(_snap(transport=transport)) is None

    def test_unknown_transport(self):
        assert snapshot_problem(_snap(transport="USB"))[0] == "json"

    @pytest.mark.parametrize("missing", ["format_version", "camera", "captured_at", "transport", "properties"])
    def test_required_fields(self, missing):
        doc = _snap()
        del doc[missing]
        assert snapshot_problem(doc)[0] == "json"

    @pytest.mark.parametrize("entry", [{"value": 1}, {"code": 1}, {"code": -1, "value": 1},
                                       {"code": 2 ** 32, "value": 1}, {"code": True, "value": 1},
                                       {"code": "1", "value": 1}, 5, None])
    def test_property_entries(self, entry):
        assert snapshot_problem(_snap(properties={"a": entry}))[0] == "json"

    def test_property_code_boundary(self):
        assert snapshot_problem(_snap(properties={"a": {"code": 2 ** 32 - 1, "value": 1}})) is None
        assert snapshot_problem(_snap(properties={"a": {"code": 0, "value": 1}})) is None

    def test_camera_fields_must_be_strings(self):
        for key in ("model", "serial", "firmware"):
            cam = {"model": "Z 9", "serial": "S", "firmware": "1"}
            cam[key] = 5
            assert snapshot_problem(_snap(camera=cam))[0] == "json"

    def test_not_an_object(self):
        for doc in ([], "x", None, 5):
            assert snapshot_problem(doc)[0] == "json"


class TestSnapshotBytesProblem:
    def _bytes(self, **over):
        return json.dumps(_snap(**over)).encode()

    def test_valid(self):
        assert snapshot_bytes_problem(self._bytes()) is None

    def test_not_utf8_is_an_io_problem(self):
        assert snapshot_bytes_problem(b"\xff\xfe{}")[0] == "io"

    def test_not_json_is_a_json_problem(self):
        assert snapshot_bytes_problem(b"{ nope")[0] == "json"
        assert snapshot_bytes_problem(b"")[0] == "json"

    @pytest.mark.parametrize("token", ["NaN", "Infinity", "-Infinity"])
    def test_json_constants_python_accepts_and_serde_does_not(self, token):
        raw = self._bytes().replace(b'"value": 1', f'"value": {token}'.encode())
        assert token.encode() in raw
        assert snapshot_bytes_problem(raw)[0] == "json"

    def test_absurdly_deep_nesting_is_a_problem_not_a_crash(self):
        assert snapshot_bytes_problem(b"[" * 100000 + b"]" * 100000)[0] == "json"


class TestFirmwareMetaProblem:
    def _meta(self, **over):
        doc = {"format_version": 1, "model": "Z 9", "firmware_version": "5.31", "archived_at": "t",
               "archived_by": "x", "bin_sha256": "ab" * 32, "bin_size_bytes": 10, "source_path": "/x",
               "canonical_snapshot_path": None, "notes": None}
        doc.update(over)
        return json.dumps(doc).encode()

    def test_valid(self):
        assert firmware_meta_problem(self._meta(), "Z_9", "5.31") is None

    def test_extra_keys_and_a_provenance_are_fine(self):
        assert firmware_meta_problem(self._meta(provenance={"method": "manual"}, extra=1), "Z_9", "5.31") is None

    @pytest.mark.parametrize("key", ["format_version", "model", "firmware_version", "bin_sha256", "bin_size_bytes"])
    def test_required_keys(self, key):
        doc = json.loads(self._meta())
        del doc[key]
        assert firmware_meta_problem(json.dumps(doc).encode(), "Z_9", "5.31")

    @pytest.mark.parametrize("over", [
        {"format_version": 2}, {"format_version": 0}, {"format_version": True}, {"format_version": "1"},
        {"model": ""}, {"model": 5}, {"firmware_version": ""}, {"bin_sha256": ""}, {"bin_sha256": 5},
        {"bin_size_bytes": -1}, {"bin_size_bytes": "10"}, {"bin_size_bytes": True}, {"bin_size_bytes": 1.5},
    ])
    def test_wrong_values(self, over):
        assert firmware_meta_problem(self._meta(**over), "Z_9", "5.31")

    def test_zero_size_is_a_size(self):
        assert firmware_meta_problem(self._meta(bin_size_bytes=0), "Z_9", "5.31") is None

    def test_the_directory_must_be_the_models_slug_and_the_version(self):
        assert firmware_meta_problem(self._meta(), "Z_8", "5.31")
        assert firmware_meta_problem(self._meta(), "Z_9", "5.32")
        assert firmware_meta_problem(self._meta(model="Z6_3"), "Z6_3", "5.31") is None

    def test_not_json_or_not_an_object(self):
        for raw in (b"{ nope", b"[]", b"\xff", b""):
            assert firmware_meta_problem(raw, "Z_9", "5.31")


# ── fmt_cap_value ─────────────────────────────────────────────────────────────

def _z9_wb_tune(index, value=0.0):
    """A stepped range as a Z 9 reports WBTune*: 0..1224 in 1225 steps, with
    `value` left at 0.0 and the position in `value_index` (F18)."""
    return {"value": value, "default": 0.0, "value_index": index, "default_index": 0,
            "lower": 0.0, "upper": 1224.0, "steps": 1225}


class TestFmtCapValueFixtureCrossCheck:
    """Loads tests/fixtures/capability_value_shapes.json — the same file
    src/sdk.rs's tests assert is exactly what decode_value()/
    decode_enum_values() produce for each branch — and runs fmt_cap_value
    against every entry."""

    @pytest.fixture
    def shapes(self):
        data = json.loads(_FIXTURE.read_text())
        data.pop("_comment", None)
        return data

    def test_every_shape_renders_without_raising(self, shapes):
        for name, value in shapes.items():
            result = fmt_cap_value(value)
            assert isinstance(result, str) and result, f"{name}: expected non-empty str, got {result!r}"

    def test_unsigned_enum_shape(self, shapes):
        assert fmt_cap_value(shapes["unsigned_enum"]) == "2"

    def test_packed_string_enum_shape(self, shapes):
        assert fmt_cap_value(shapes["packed_string_enum"]) == "RAW"

    def test_range_shape(self, shapes):
        # value 1.0, value_index 18 of 31 steps over -5..5 -> 1.0 either way
        assert fmt_cap_value(shapes["range"]) == "1.0"


class TestFmtCapValueRange:
    def test_a_stepped_range_shows_its_index_position_not_its_value(self):
        # The Z 9 WBTune* shape: `value` is 0.0 whatever the position.
        assert fmt_cap_value(_z9_wb_tune(1224)) == "1224.0"
        assert fmt_cap_value(_z9_wb_tune(612)) == "612.0"
        assert fmt_cap_value(_z9_wb_tune(0)) == "0.0"

    def test_position_is_lower_plus_index_times_step(self):
        v = {"value": 0.0, "value_index": 750, "lower": 2500.0, "upper": 10000.0, "steps": 751}
        assert fmt_cap_value(v) == "10000.0"
        v["value_index"] = 1
        assert fmt_cap_value(v) == "2510.0"

    def test_the_junk_value_is_ignored_on_a_stepped_range(self):
        assert fmt_cap_value(_z9_wb_tune(7, value=99.0)) == "7.0"

    def test_index_at_below_above_the_last_step(self):
        assert fmt_cap_value(_z9_wb_tune(1223)) == "1223.0"
        assert fmt_cap_value(_z9_wb_tune(1224)) == "1224.0"            # steps - 1: the last
        past = fmt_cap_value(_z9_wb_tune(1225))                         # steps: one past the end
        assert "1225" in past and "past" in past and not past[0].isdigit()
        assert "1226" in fmt_cap_value(_z9_wb_tune(1226))

    def test_the_junk_value_is_not_shown_for_an_index_past_the_end(self):
        assert "0.0" != fmt_cap_value(_z9_wb_tune(1225))

    @pytest.mark.parametrize("index", [-1, "5", 5.0, 5.5, None, True, [5]])
    def test_an_index_that_is_not_a_position_has_no_number_and_does_not_raise(self, index):
        shown = fmt_cap_value(_z9_wb_tune(index))
        assert shown.startswith("[stepped range")

    def test_missing_index_has_no_number(self):
        v = _z9_wb_tune(0)
        del v["value_index"]
        assert fmt_cap_value(v).startswith("[stepped range")

    def test_two_steps_is_the_least_that_is_stepped(self):
        two = {"value": 0.0, "value_index": 1, "lower": 0.0, "upper": 1.0, "steps": 2}
        one = {"value": 0.25, "value_index": 1, "lower": 0.0, "upper": 1.0, "steps": 1}
        assert fmt_cap_value(two) == "1.0"          # stepped: from the index
        assert fmt_cap_value(one) == "0.25"         # not stepped: its value

    def test_no_span_is_not_stepped(self):
        v = {"value": 3.0, "value_index": 4, "lower": 5.0, "upper": 5.0, "steps": 10}
        assert fmt_cap_value(v) == "3.0"

    def test_a_continuous_range_shows_its_value(self):
        v = {"value": 3.5, "value_index": 0, "lower": 0.0, "upper": 10.0, "steps": 0}
        assert fmt_cap_value(v) == "3.5"

    def test_range_without_steps_shows_its_value(self):
        assert fmt_cap_value({"value": 2.5, "lower": 0.0, "upper": 10.0}) == "2.5"
        assert fmt_cap_value({"value": 2.5, "lower": 0.0, "upper": 10.0, "steps": None}) == "2.5"

    def test_range_without_a_value_says_so(self):
        shown = fmt_cap_value({"lower": 0.0, "upper": 10.0, "steps": 0})
        assert "missing" in shown

    def test_non_numeric_value_is_shown_not_defaulted(self):
        assert "'x'" in fmt_cap_value({"value": "x", "lower": 0.0, "upper": 10.0})

    def test_lower_without_upper_is_not_a_range(self):
        # Pinned: the old code fell back to str(v.get("value", v)); now it is shown as it is.
        shown = fmt_cap_value({"lower": 0.0, "value": 3.0})
        assert shown == '{"lower": 0.0, "value": 3.0}'

    def test_upper_without_lower_is_not_a_range(self):
        assert fmt_cap_value({"upper": 9.0, "value": 3.0}) == '{"upper": 9.0, "value": 3.0}'

    def test_range_dict_midpoint_still_works(self):
        v = {"lower": -5.0, "upper": 5.0, "steps": 31, "value": 0.0,
             "value_index": 15, "default": 0.0, "default_index": 15}
        assert fmt_cap_value(v) == "0.0"


class TestFmtCapValueEnum:
    LABELS = ["JPEG Basic", "JPEG Normal", "JPEG Fine", "RAW", "RAW+Basic", "RAW+Fine"]

    def _enum(self, index, values=None, **extra):
        v = {"elem_type": 7, "value_index": index, "elem_count": 6, "elem_bytes": 12,
             "default_index": 0, "values": self.LABELS if values is None else values}
        v.update(extra)
        return v

    def test_packed_string_enum(self):
        assert fmt_cap_value(self._enum(3)) == "RAW"

    def test_integer_enum(self):
        v = {"elem_type": 2, "value_index": 2, "elem_count": 4, "elem_bytes": 4,
             "default_index": 0, "values": [0, 1, 2, 3]}
        assert fmt_cap_value(v) == "2"

    def test_index_at_below_above_the_end(self):
        assert fmt_cap_value(self._enum(5)) == "RAW+Fine"                   # len - 1
        assert fmt_cap_value(self._enum(6)) == "[index 6 / 6]"              # len
        assert fmt_cap_value(self._enum(7)) == "[index 7 / 6]"
        assert fmt_cap_value(self._enum(0)) == "JPEG Basic"

    def test_out_of_bounds_index(self):
        assert "99" in fmt_cap_value(self._enum(99, ["Auto"]))

    @pytest.mark.parametrize("index", [-1, "3", 3.0, None, True, [3]])
    def test_an_index_that_is_not_a_position_is_shown_and_does_not_raise(self, index):
        shown = fmt_cap_value(self._enum(index))
        assert shown.startswith("[enum index missing or invalid")
        assert "RAW" not in shown

    def test_negative_index_is_not_python_indexing_from_the_end(self):
        assert fmt_cap_value(self._enum(-1)) != "RAW+Fine"

    def test_missing_values_is_distinguishable_from_an_empty_list(self):
        no_values = {"elem_type": 7, "value_index": 2}
        empty = self._enum(2, [])
        assert "without a values list" in fmt_cap_value(no_values)
        assert fmt_cap_value(empty) == "[index 2 / 0]"
        assert fmt_cap_value(no_values) != fmt_cap_value(empty)

    def test_unsupported_enum_type_fallback_does_not_crash(self):
        # decode_enum_values (Rust) falls back to a JSON object for an array-element type it
        # doesn't decode. This used to raise KeyError (dict[0]).
        v = {"elem_type": 99, "value_index": 0, "elem_count": 1,
             "elem_bytes": 4, "default_index": 0, "values": {"_unsupported_enum_type": 99}}
        assert "_unsupported_enum_type" in fmt_cap_value(v)

    def test_invalid_physical_bytes_fallback_does_not_crash(self):
        v = {"elem_type": 7, "value_index": 0, "elem_count": 2,
             "elem_bytes": -1, "default_index": 0, "values": {"_invalid_physical_bytes": -1}}
        assert "_invalid_physical_bytes" in fmt_cap_value(v)

    def test_values_that_is_neither_list_nor_object(self):
        assert "not a list" in fmt_cap_value({"value_index": 0, "values": "x"})

    def test_enum_without_elem_type_still_indexes(self):
        assert fmt_cap_value({"value_index": 1, "values": ["a", "b"]}) == "b"


class TestFmtCapValueOther:
    def test_scalar_int(self):
        assert fmt_cap_value(100) == "100"

    def test_scalar_float(self):
        assert fmt_cap_value(5.6) == "5.6"

    def test_bool(self):
        assert fmt_cap_value(True) == "Yes"
        assert fmt_cap_value(False) == "No"

    def test_string_passthrough(self):
        assert fmt_cap_value("USB") == "USB"

    def test_null_is_shown_as_null(self):
        assert fmt_cap_value(None) == "null"

    def test_datetime_array_and_unknown_shapes_are_readable(self):
        assert fmt_cap_value({"_type": "datetime"}) == "(datetime: not captured)"
        assert fmt_cap_value({"_type": "array"}) == "(array: not captured)"
        assert fmt_cap_value({"_unknown_type": 17}) == "(unknown capability type 17)"

    def test_an_unrecognised_object_is_shown_as_compact_json(self):
        assert fmt_cap_value({"b": 1, "a": [2]}) == '{"a": [2], "b": 1}'

    def test_a_long_unrecognised_object_is_cut_with_an_ellipsis(self):
        shown = fmt_cap_value({"k": "x" * 500})
        assert shown.endswith("…") and len(shown) < 260

    # ── list rendering boundary (6 vs 7 items) ────────────────────────────

    def test_list_empty(self):
        assert fmt_cap_value([]) == "[]"

    def test_list_exactly_six(self):
        assert fmt_cap_value([1, 2, 3, 4, 5, 6]) == "[1, 2, 3, 4, 5, 6]"

    def test_list_exactly_seven(self):
        result = fmt_cap_value([1, 2, 3, 4, 5, 6, 7])
        assert "…" in result and "7 items" in result


class TestPropertyRows:
    def test_sorted_and_prefix_stripped(self):
        rows = property_rows({"kNkMAIDCapability_B": {"code": 2, "value": "x"},
                              "kNkMAIDCapability_A": {"code": 1, "value": 1}})
        assert rows == [("A", "1", "0x0001"), ("B", "x", "0x0002")]

    def test_missing_code_is_not_shown_as_zero(self):
        assert property_rows({"a": {"value": 1}}) == [("a", "1", "(missing)")]

    def test_missing_value_is_not_shown_as_none(self):
        assert property_rows({"a": {"code": 1}}) == [("a", "(missing)", "0x0001")]

    def test_invalid_code_is_missing(self):
        for code in (-1, "5", 1.5, True, None, 2 ** 32):
            assert property_rows({"a": {"code": code, "value": 1}})[0][2] == "(missing)"

    def test_a_null_value_is_not_missing(self):
        assert property_rows({"a": {"code": 1, "value": None}}) == [("a", "null", "0x0001")]

    def test_malformed_entry_does_not_raise(self):
        rows = property_rows({"a": 5, "b": None, "c": [1]})
        assert [r[0] for r in rows] == ["a", "b", "c"]
        assert all(r[1].startswith("[malformed entry") and r[2] == "(missing)" for r in rows)

    def test_properties_not_an_object(self):
        assert property_rows([1])[0][1].startswith("[not an object")
        assert property_rows(None)[0][1].startswith("[not an object")

    def test_a_real_snapshot_from_the_fixture(self):
        doc, problem = read_snapshot_file(_VALIDITY / "snapshots" / "ok.json")
        assert problem is None
        assert property_rows(doc["properties"]) == [("A", "1", "0x0001"), ("B", "x", "0x0002")]


# ── ls --json ─────────────────────────────────────────────────────────────────

class TestParseListing:
    def test_the_validity_directory(self):
        listing = parse_listing(_read("ls_snapshot_validity.json"))
        assert [r.file for r in listing.rows] == ["ok.json", "ok_extra_key_null_label.json"]
        assert {u.file: u.error_kind for u in listing.unreadable} == {
            "bad_transport.json": "json", "broken.json": "json", "future_version.json": "unsupported_version",
            "no_properties.json": "json", "no_transport.json": "json", "no_version.json": "json",
            "property_without_code.json": "json"}
        ok = listing.rows[0]
        assert (ok.model, ok.serial, ok.firmware, ok.label, ok.property_count) == ("Z 9", "0000003023668", "5.31", "fixture", 2)
        assert ok.is_reference is False
        assert listing.rows[1].label == ""            # null label

    def test_unreadable_files_are_counted_and_reported_not_dropped(self):
        listing = parse_listing(_read("ls_snapshot_validity.json"))
        assert "7 snapshot file(s) unreadable" in listing_notes(listing)

    def test_an_unreadable_directory_is_not_no_snapshots(self):
        doc = {"dir": "/d/snapshots", "dir_exists": True, "entries": [], "skipped_dir_entries": 3,
               "reference_read_failures": 2}
        notes = listing_notes(parse_listing(json.dumps(doc)))
        assert "3 directory entries could not be read" in notes
        assert "2 reference file(s) could not be read" in notes

    def test_a_clean_listing_has_no_notes(self):
        doc = {"dir": "/d/snapshots", "dir_exists": True, "entries": []}
        assert listing_notes(parse_listing(json.dumps(doc))) == ""

    @pytest.mark.parametrize("bad", [
        "", "not json", "[]", '"x"', "{}", '{"dir": "d", "entries": {}}', '{"entries": []}',
        'InitializeSDK Execution duration: 9\n{"dir": "d", "dir_exists": true, "entries": []}',
        '{"dir": "d", "entries": [5]}',
        '{"dir": "d", "entries": [{"file": "a.json", "status": "weird"}]}',
        '{"dir": "d", "entries": [{"file": "a.json", "status": "ok", "serial": "S", "property_count": 1}]}',
        '{"dir": "d", "entries": [{"file": "a.json", "status": "ok", "model": "Z", "serial": "S", "property_count": "1"}]}',
        '{"dir": "d", "entries": [{"status": "unreadable", "error_kind": "json"}]}',
        '{"dir": "d", "entries": [], "skipped_dir_entries": "3"}',
    ])
    def test_malformed_documents_raise_before_anything_is_returned(self, bad):
        with pytest.raises(CliOutputError):
            parse_listing(bad)

    def test_reference_marks(self):
        listing = parse_listing(_read("ls_reference_parity.json"))
        marks = listing_ref_marks(listing)
        assert marks[("Z 9", "ABC123")] == ["abc_ref_source.json", "abc_ref_twin.json"]
        assert marks[("Z6_3", "id-1")] == ["id1_z6.json"]
        assert ("Z 9", "id-1") not in marks


class TestCamerasFromTheListing:
    LISTING = parse_listing(_read("ls_reference_parity.json"))

    def test_an_id_serial_shared_by_two_models_is_two_cameras(self):
        cams = known_cameras(self.LISTING)
        ids = [camera_key(c) for c in cams if c["serial"] == "id-1"]
        assert ids == [("Z 9", "id-1"), ("Z6_3", "id-1")]

    def test_rows_are_per_model_and_serial(self):
        assert [r.file for r in rows_for_camera(self.LISTING, "Z 9", "id-1")] == ["id1_z9.json"]
        assert [r.file for r in rows_for_camera(self.LISTING, "Z6_3", "id-1")] == ["id1_z6.json"]
        assert rows_for_camera(self.LISTING, "Z 5", "id-1") == []

    def test_reference_flags_match_the_shared_reference_fixture(self):
        # tests/fixtures/reference_parity/expected.json, keyed by serial: the CLI's
        # answer (captured in the fixture) read through the GUI's own parser.
        expected = json.loads((_REFERENCE_PARITY / "expected.json").read_text(encoding="utf-8"))
        for serial, want in expected.items():
            got = {r.file: r.is_reference for r in self.LISTING.rows if r.serial == serial}
            assert got == want, serial

    def test_the_newest_snapshots_firmware_is_the_cameras(self):
        doc = {"dir": "d", "dir_exists": True, "entries": [
            {"file": "a.json", "status": "ok", "model": "Z 9", "serial": "S", "firmware": "5.00",
             "label": None, "captured_at": "2026-01-01T00:00:00Z", "property_count": 1, "is_reference": False},
            {"file": "b.json", "status": "ok", "model": "Z 9", "serial": "S", "firmware": "5.31",
             "label": None, "captured_at": "2026-06-01T00:00:00Z", "property_count": 1, "is_reference": False}]}
        assert known_cameras(parse_listing(json.dumps(doc))) == [{"model": "Z 9", "serial": "S", "firmware": "5.31"}]

    def test_rows_newest_first_with_a_stable_tiebreak_and_null_labels(self):
        def row(name, at, label):
            return {"file": name, "status": "ok", "model": "Z 9", "serial": "S", "firmware": "1",
                    "label": label, "captured_at": at, "property_count": 1, "is_reference": False}
        doc = {"dir": "d", "dir_exists": True, "entries": [
            row("a.json", "2026-01-01T00:00:00Z", None), row("b.json", "2026-01-01T00:00:00Z", "x"),
            row("c.json", "2026-03-01T00:00:00Z", None), row("d.json", "2026-02-01T00:00:00Z", "y")]}
        got = [r.file for r in rows_for_camera(parse_listing(json.dumps(doc)), "Z 9", "S")]
        # same-second pair (a, b) with a null and a string label sorts without a TypeError
        assert got == ["c.json", "d.json", "b.json", "a.json"]

    def test_a_listing_with_no_snapshots_has_no_cameras(self):
        listing = parse_listing('{"dir": "d", "dir_exists": false, "entries": []}')
        assert known_cameras(listing) == []

    def test_camera_label(self):
        assert camera_label({"model": "Z 9", "serial": "S"}) == "  Z 9  ·  S"


# ── firmware ls --json ────────────────────────────────────────────────────────

class TestFirmwareListing:
    LISTING = parse_firmware_listing(_read("firmware_ls.json"))

    def test_valid_entries_and_unreadable_entries_are_separate(self):
        assert [(r.model, r.version) for r in self.LISTING.rows] == [("Z 5", "1.10"), ("Z 9", "5.31"), ("Z6_3", "2.00")]
        assert {u.error_kind for u in self.LISTING.unreadable} == {
            "unsupported_version", "incomplete_archive", "metadata_corrupt"}

    def test_a_missing_binary_is_shown_not_listed_as_archived(self):
        rows = {r[1]: r for r in firmware_display_rows(self.LISTING) if r[1] != "(unreadable)"}
        assert rows["Z 5"][4] == "BINARY MISSING"
        assert rows["Z 9"][4] == "ok"

    def test_unreadable_entries_carry_their_error_kind(self):
        bad = [r for r in firmware_display_rows(self.LISTING) if r[1] == "(unreadable)"]
        assert sorted(r[4] for r in bad) == ["unreadable: incomplete_archive",
                                             "unreadable: metadata_corrupt", "unreadable: unsupported_version"]

    def test_the_iid_is_the_directory_the_cli_reported_and_location_is_relative(self):
        row = next(r for r in firmware_display_rows(self.LISTING) if r[1] == "Z6_3")
        assert row[0] == "/data/firmware/Z6_3/2.00"
        assert row[3] == "Z6_3/2.00"

    def test_the_legacy_flat_layout_is_not_a_thing_the_gui_lists(self):
        doc = {"dir": "/d/firmware", "dir_exists": True, "entries": [], "skipped_dir_entries": 0}
        assert firmware_display_rows(parse_firmware_listing(json.dumps(doc))) == []

    @pytest.mark.parametrize("bad", [
        "", "[]", "{}", '{"dir": "d", "entries": [{"status": "ok", "directory": "x", "model": "m", "version": "1"}]}',
        '{"dir": "d", "entries": [{"status": "ok", "directory": "x", "model": "m", "version": "1", "bin_present": 1}]}',
        '{"dir": "d", "entries": [{"status": "nope", "directory": "x"}]}',
        '{"dir": "d", "entries": [{"status": "unreadable"}]}',
    ])
    def test_malformed_documents_raise(self, bad):
        with pytest.raises(CliOutputError):
            parse_firmware_listing(bad)


# ── settings show --json / discover --json ────────────────────────────────────

class TestParseSettings:
    def test_the_captured_document(self):
        info = parse_settings_show(_read("settings_show.json"))
        assert info.data_dir == "/Volumes/Files/nikon-data"
        assert info.effective_data_dir == "/Volumes/Files/nikon-data"
        assert info.settings_exist and "window" in info.keys

    def test_unset_data_dir_uses_the_effective_one(self):
        doc = {"settings_path": "/s.json", "settings_exist": False, "data_dir": None,
               "effective_data_dir": "/Users/u/Library/Application Support/net.blw.fleet", "keys": {}}
        info = parse_settings_show(json.dumps(doc))
        assert info.data_dir is None and info.effective_data_dir.endswith("net.blw.fleet")

    @pytest.mark.parametrize("bad", [
        "", "[]", "{}", '{"settings_path": "p", "effective_data_dir": ""}',
        '{"settings_path": "p", "effective_data_dir": 5}',
        '{"settings_path": "p", "effective_data_dir": "/d", "data_dir": 5}',
        '{"settings_path": "p", "effective_data_dir": "/d", "keys": []}',
    ])
    def test_malformed_documents_raise(self, bad):
        with pytest.raises(CliOutputError):
            parse_settings_show(bad)


class TestParseDiscover:
    # gui/fixtures/discover.json is written by hand from the contract (the
    # CLI's discover needs a camera): {model, firmware, serial, sdk_id,
    # usb_serial, known_model, baseline}.
    def test_the_documented_shape(self):
        cams = parse_discover(_read("discover.json"))
        assert [camera_key(c) for c in cams] == [("Z 9", "0000003023668"), ("Z6_3", "id-2"), ("Z 5", "6000001")]
        assert [c["baseline"] for c in cams] == [True, True, False]

    def test_zero_cameras(self):
        assert parse_discover('{"cameras": []}') == []

    @pytest.mark.parametrize("bad", [
        "", "[]", "{}", '{"cameras": {}}', '{"cameras": [5]}', '{"cameras": [{"model": "Z 9"}]}',
        '{"cameras": [{"model": "Z 9", "serial": 5}]}',
        'InitializeSDK Execution duration: 209\n{"cameras": []}',
    ])
    def test_malformed_documents_raise(self, bad):
        with pytest.raises(CliOutputError):
            parse_discover(bad)

    def test_one_bad_camera_means_none_are_returned(self):
        # Validated before assigning: a KeyError on the second camera used to
        # leave the first one in the list.
        doc = '{"cameras": [{"model": "Z 9", "serial": "S"}, {"model": "Z 5"}]}'
        with pytest.raises(CliOutputError):
            parse_discover(doc)


# ── argv ──────────────────────────────────────────────────────────────────────

_DASHED = "-guest"


class TestArgv:
    def test_global_options_use_the_equals_form(self):
        assert global_argv("/d") == ["--no-usb-reset", "--data-dir=/d"]
        assert global_argv("-odd") == ["--no-usb-reset", "--data-dir=-odd"]

    def test_snapshot(self):
        assert snapshot_argv("S1", "") == ["snapshot", "--serial=S1"]
        assert snapshot_argv("S1", "wedding") == ["snapshot", "--serial=S1", "--label=wedding"]

    def test_a_label_or_serial_that_starts_with_a_dash_is_one_argument(self):
        assert snapshot_argv("-S", "-guest") == ["snapshot", "--serial=-S", "--label=-guest"]

    def test_firmware_add_puts_values_in_the_equals_form_and_the_path_after_a_double_dash(self):
        assert firmware_add_argv("/f/x.bin", "Z 9", "5.31") == [
            "firmware", "add", "--model=Z 9", "--version=5.31", "--", "/f/x.bin"]
        assert firmware_add_argv("-x.bin", "-Z", "-1", force=True) == [
            "firmware", "add", "--model=-Z", "--version=-1", "--force", "--", "-x.bin"]

    def test_firmware_browse_model_follows_a_double_dash(self):
        assert firmware_browse_argv("-Z 9") == ["firmware", "browse", "--", "-Z 9"]

    def test_json_modes(self):
        assert ls_argv() == ["ls", "--json"]
        assert firmware_ls_argv() == ["firmware", "ls", "--json"]
        assert settings_show_argv() == ["settings", "show", "--json"]

    def test_set_data_dir(self):
        assert set_data_dir_argv("/new") == ["settings", "set-data-dir", "--", "/new"]
        assert set_data_dir_argv("-x") == ["settings", "set-data-dir", "--", "-x"]
        assert set_data_dir_argv("") == ["settings", "set-data-dir", "--unset"]
        assert set_data_dir_argv(None) == ["settings", "set-data-dir", "--unset"]

    def test_ref_set_builds_an_absolute_path_from_a_plain_name(self):
        assert ref_set_argv("/d", "a.json") == ["ref", "set", "--", "/d/snapshots/a.json"]

    @pytest.mark.parametrize("name", ["", ".", "..", "../x.json", "a/b.json", "a\\b.json", "a\0.json", "/etc/passwd"])
    def test_ref_set_refuses_a_name_that_is_not_one_plain_component(self, name):
        with pytest.raises(ValueError):
            ref_set_argv("/d", name)

    def test_vendor_read(self):
        assert vendor_read_argv(0xD053, None) == ["vendor-read", "0xd053"]
        assert vendor_read_argv(0xD053, "S") == ["vendor-read", "0xd053", "--serial=S"]
        assert vendor_read_argv(0x5, "") == ["vendor-read", "0x0005"]

    VALUES = {"profile_name": "P", "ssid_24ghz": "s24", "ssid_5ghz": "s5", "host": "h", "port": 21,
              "username": "u", "password": "secret"}

    def test_vendor_write_ftp_shape_and_flag_order(self):
        assert vendor_write_ftp_argv(self.VALUES, None, dry_run=False) == [
            "vendor-write-ftp", "--profile-name=P", "--ssid-24ghz=s24", "--ssid-5ghz=s5", "--host=h",
            "--port=21", "--username=u", "--password-stdin"]
        assert vendor_write_ftp_argv(self.VALUES, "S", dry_run=True)[-2:] == ["--serial=S", "--dry-run"]

    def test_the_password_is_never_an_argument(self):
        argv = vendor_write_ftp_argv(self.VALUES, "S", dry_run=False)
        assert not any("secret" in a for a in argv)
        assert "--password" not in argv and "--password-stdin" in argv

    def test_dashed_values_in_every_field_stay_one_argument_each(self):
        vals = {k: (_DASHED if k != "port" else 21) for k in self.VALUES} | {"port": 2121}
        argv = vendor_write_ftp_argv(vals, "-S", dry_run=True)
        assert argv == [
            "vendor-write-ftp", "--profile-name=-guest", "--ssid-24ghz=-guest", "--ssid-5ghz=-guest",
            "--host=-guest", "--port=2121", "--username=-guest", "--password-stdin",
            "--serial=-S", "--dry-run"]
        assert not any(a.startswith("-") and "=" not in a and a not in ("--password-stdin", "--dry-run") for a in argv)

    def test_flag_helper(self):
        assert flag("--x", "-y") == "--x=-y"


class TestFtpFormValues:
    RAW: ClassVar[dict] = {"profile_name": " P ", "ssid_24ghz": "s24 ", "ssid_5ghz": " s5", "host": "h ",
                           "port": " 21 ", "username": " u", "password": " pw "}

    def test_text_and_password_are_sent_exactly_as_typed(self):
        v = ftp_form_values(self.RAW)
        assert v["profile_name"] == " P " and v["ssid_24ghz"] == "s24 " and v["ssid_5ghz"] == " s5"
        assert v["host"] == "h " and v["username"] == " u" and v["password"] == " pw "

    def test_a_trailing_space_reaches_the_argv_unchanged(self):
        argv = vendor_write_ftp_argv(ftp_form_values(self.RAW), None, dry_run=False)
        assert "--ssid-24ghz=s24 " in argv and "--profile-name= P " in argv and "--username= u" in argv

    def test_the_port_is_parsed_strictly_with_its_whitespace_ignored(self):
        assert ftp_form_values(self.RAW)["port"] == 21

    def test_empty_and_whitespace_only_fields_pass_through_for_the_cli_to_refuse(self):
        v = ftp_form_values({**self.RAW, "profile_name": "  ", "password": ""})
        assert v["profile_name"] == "  " and v["password"] == ""

    @pytest.mark.parametrize("port", ["0", "65536", "2_1", "+21", "", "x"])
    def test_a_bad_port_raises(self, port):
        with pytest.raises(ValueError):
            ftp_form_values({**self.RAW, "port": port})


# ── Results ───────────────────────────────────────────────────────────────────

class TestBrowseOutcome:
    def test_success_shows_stdout(self):
        ok, text = browse_outcome(0, "  archived Z 9 5.33\n", "")
        assert ok and text == "archived Z 9 5.33"

    def test_failure_shows_stderr_then_stdout(self):
        ok, text = browse_outcome(1, "Download it by hand instead:\n  1. open X\n", "could not open the embedded browser: no\n")
        assert not ok
        assert text.startswith("could not open the embedded browser: no")
        assert text.endswith("1. open X")

    def test_failure_with_no_output_still_says_something(self):
        assert browse_outcome(2, "", "") == (False, "fleet exited 2")

    def test_nonzero_with_only_stdout(self):
        assert browse_outcome(1, "hint\n", "") == (False, "hint")


class TestJoinOutput:
    def test_both_streams_are_kept(self):
        assert join_output("out\n", "warning: x\n") == "out\nwarning: x"

    def test_either_alone(self):
        assert join_output("out", "") == "out"
        assert join_output("", "warn") == "warn"
        assert join_output("", "") == ""


def test_exit_status_for_already_archived_is_the_documented_one():
    assert EXIT_ALREADY_ARCHIVED == 3


def test_exit_status_for_an_indeterminate_write_is_the_documented_one():
    assert EXIT_WRITE_INDETERMINATE == 4


class TestFtpWriteFailureText:
    MAY = "The camera may have changed: check the FTP profile in the camera's menu"

    @pytest.mark.parametrize("rc", [4, 3, 5, 101, 139, 255, -9, -11, -15, 0])
    def test_every_status_but_1_and_2_may_have_changed_the_camera(self, rc):
        text = ftp_write_failure_text(rc, False, "detail")
        assert text.startswith("detail") and self.MAY in text

    @pytest.mark.parametrize("rc", [None])
    def test_no_exit_status_at_all_may_have_changed_the_camera(self, rc):
        # a CliResult(None, problem=...): killed on the timeout, or the wait itself failed
        assert self.MAY in ftp_write_failure_text(rc, True, "timed out after 180 s")
        assert self.MAY in ftp_write_failure_text(rc, False, "talking to nikon-fleet failed: broken pipe")

    @pytest.mark.parametrize("rc", [1, 2])
    def test_only_1_and_2_are_plain_failures_and_the_text_is_shown_once(self, rc):
        assert ftp_write_failure_text(rc, False, "Error: camera busy") == "Error: camera busy"

    def test_a_timeout_overrides_a_plain_status(self):
        assert self.MAY in ftp_write_failure_text(1, True, "detail")


class TestIdFallbackAndReferenceName:
    @pytest.mark.parametrize("serial,expected", [
        ("id-1", True), ("id-0", True), ("id-12345", True), ("id-", False), ("id-1a", False), ("ID-1", False),
        ("id--1", False), ("id-\u0661", False), ("0000003023668", False), ("", False), ("xid-1", False),
    ])
    def test_is_id_fallback_matches_the_rust_rule(self, serial, expected):
        assert is_id_fallback(serial) is expected

    def test_reference_file_name_is_slug_underscore_serial(self):
        assert reference_file_name("Z 9", "0000003023668") == "Z_9_0000003023668.json"
        assert reference_file_name("Z6_3", "id-1") == "Z6_3_id-1.json"
        assert reference_file_name("Z 6 III", "S") == "Z_6_III_S.json"

    @pytest.mark.parametrize("model,serial", [("Z 9", "../x"), ("Z 9", "a/b"), ("Z 9", ""), ("Z 9", "."), ("", "S"),
                                              ("..", "S"), ("a\\b", "S"), ("Z 9", "a\0b"), (None, "S"), ("Z 9", 5)])
    def test_reference_file_name_refuses_a_non_plain_value(self, model, serial):
        assert reference_file_name(model, serial) is None


# ── against the real binary (no camera is touched) ────────────────────────────

def _run(real_cli, argv, stdin=None):
    return subprocess.run([real_cli, *argv], input=stdin, capture_output=True, text=True, timeout=60)


class TestAgainstTheRealCli:
    """`vendor-write-ftp --dry-run` encodes and prints; it returns before any
    camera is looked for (src/main.rs cmd_vendor_write_ftp)."""

    GOOD = {"profile_name": "P", "ssid_24ghz": "s24", "ssid_5ghz": "s5", "host": "h", "port": 21,
            "username": "u", "password": "pw"}

    def test_dashed_values_in_every_field_are_accepted(self, real_cli):
        vals = {k: _DASHED for k in self.GOOD} | {"port": 21}
        r = _run(real_cli, vendor_write_ftp_argv(vals, None, dry_run=True), "pw")
        assert r.returncode == 0, r.stderr
        assert "Would write" in r.stdout

    def test_the_space_form_the_gui_used_to_send_is_refused_by_clap(self, real_cli):
        # Control for the test above: with `--flag value` the same values exit 2.
        r = _run(real_cli, ["vendor-write-ftp", "--profile-name", "-guest", "--ssid-24ghz", "s", "--ssid-5ghz", "s",
                            "--host", "h", "--username", "u", "--password-stdin", "--dry-run"], "pw")
        assert r.returncode == 2

    def test_every_flag_the_gui_sends_is_accepted(self, real_cli):
        r = _run(real_cli, vendor_write_ftp_argv(self.GOOD, "S", dry_run=True), "pw")
        assert r.returncode == 0, r.stderr   # --serial is accepted, and not used by a dry run

    def test_the_password_comes_from_stdin_and_is_redacted_in_the_preview(self, real_cli):
        r = _run(real_cli, vendor_write_ftp_argv(self.GOOD, None, dry_run=True), "hunter2hunter2")
        assert "hunter2" not in r.stdout and "hunter2" not in r.stderr

    def test_each_form_field_left_blank_is_refused_by_name_by_the_cli(self, real_cli):
        # Parity with `ptp_usb::FTP_PROFILE_FIELDS` (pinned in Rust by
        # `ftp_profile_missing_fields_names_come_from_the_spec_table`): the GUI
        # holds no copy of the required-field list, so this checks that every
        # field the form sends is one the CLI requires, each by its own name.
        names = []
        for key in [k for k, _f, _l in FTP_TEXT_FIELDS] + [FTP_PASSWORD_KEY]:
            vals = dict(self.GOOD, **{key: ""})
            r = _run(real_cli, vendor_write_ftp_argv(vals, None, dry_run=True), vals["password"])
            assert r.returncode == 1, (key, r.stdout, r.stderr)
            _, _, listed = r.stderr.partition("missing field(s): ")
            refused = [n.strip() for n in listed.split("\n")[0].split(",")]
            assert len(refused) == 1, (key, r.stderr)
            names.append(refused[0])
        assert len(set(names)) == len(names) == 6

    def test_all_blank_names_exactly_the_six_fields(self, real_cli):
        vals = {k: "" for k in self.GOOD} | {"port": 21}
        r = _run(real_cli, vendor_write_ftp_argv(vals, None, dry_run=True), "")
        _, _, listed = r.stderr.partition("missing field(s): ")
        assert len(listed.split("\n")[0].split(",")) == 6

    def test_the_live_listing_of_the_validity_fixture_matches_the_captured_json(self, real_cli):
        # `--data-dir` is the directory that holds `snapshots/`.
        r = _run(real_cli, [f"--data-dir={_VALIDITY}", *ls_argv()])
        assert r.returncode == 0, r.stderr
        live = parse_listing(r.stdout)
        captured = parse_listing(_read("ls_snapshot_validity.json"))
        assert live.rows == captured.rows and live.unreadable == captured.unreadable

    def test_the_live_listing_of_the_reference_fixture_matches_the_shared_expectations(self, real_cli):
        r = _run(real_cli, [f"--data-dir={_REFERENCE_PARITY}", *ls_argv()])
        assert r.returncode == 0, r.stderr
        listing = parse_listing(r.stdout)
        expected = json.loads((_REFERENCE_PARITY / "expected.json").read_text(encoding="utf-8"))
        for serial, want in expected.items():
            assert {x.file: x.is_reference for x in listing.rows if x.serial == serial} == want, serial

    def test_add_firmware_then_a_repeat_exits_with_the_documented_status(self, real_cli, tmp_path):
        binfile = tmp_path / "fw.bin"
        binfile.write_bytes(b"firmware bytes")
        data = tmp_path / "data"
        data.mkdir()
        base = [f"--data-dir={data}"]
        first = _run(real_cli, base + firmware_add_argv(str(binfile), "Z6_3", "2.00"))
        assert first.returncode == 0, first.stderr
        second = _run(real_cli, base + firmware_add_argv(str(binfile), "Z6_3", "2.00"))
        assert second.returncode == EXIT_ALREADY_ARCHIVED
        forced = _run(real_cli, base + firmware_add_argv(str(binfile), "Z6_3", "2.00", force=True))
        assert forced.returncode == 0, forced.stderr
        fw = parse_firmware_listing(_run(real_cli, base + firmware_ls_argv()).stdout)
        assert [(r.model, r.version, r.bin_present) for r in fw.rows] == [("Z6_3", "2.00", True)]

    def test_a_dashed_firmware_model_and_version_reach_the_cli(self, real_cli, tmp_path):
        binfile = tmp_path / "fw.bin"
        binfile.write_bytes(b"x")
        data = tmp_path / "data"
        data.mkdir()
        r = _run(real_cli, [f"--data-dir={data}", *firmware_add_argv(str(binfile), "-Z", "-1.00")])
        assert r.returncode == 0, r.stderr
        fw = parse_firmware_listing(_run(real_cli, [f"--data-dir={data}", *firmware_ls_argv()]).stdout)
        assert [(r.model, r.version) for r in fw.rows] == [("-Z", "-1.00")]

    def test_settings_show_json_parses(self, real_cli, tmp_path):
        env = {"NIKON_FLEET_SETTINGS": str(tmp_path / "s.json"), "PATH": "/usr/bin:/bin"}
        r = subprocess.run([real_cli, *settings_show_argv()], capture_output=True, text=True, env=env, timeout=60)
        assert r.returncode == 0, r.stderr
        info = parse_settings_show(r.stdout)
        assert info.settings_exist is False and info.data_dir is None
        set_r = subprocess.run([real_cli, *set_data_dir_argv(str(tmp_path / "d"))], capture_output=True,
                               text=True, env=env, timeout=60)
        assert set_r.returncode == 0, set_r.stderr
        info = parse_settings_show(subprocess.run([real_cli, *settings_show_argv()], capture_output=True,
                                                  text=True, env=env, timeout=60).stdout)
        assert info.data_dir == str(tmp_path / "d")
        unset = subprocess.run([real_cli, *set_data_dir_argv(None)], capture_output=True, text=True, env=env, timeout=60)
        assert unset.returncode == 0, unset.stderr
        assert parse_settings_show(subprocess.run([real_cli, *settings_show_argv()], capture_output=True,
                                                  text=True, env=env, timeout=60).stdout).data_dir is None

    def test_ref_set_refuses_a_hostile_serial_and_leaves_references_alone(self, real_cli, tmp_path):
        data = tmp_path / "data"
        (data / "snapshots").mkdir(parents=True)
        doc = json.loads((_VALIDITY / "snapshots" / "ok.json").read_text())
        doc["camera"]["serial"] = "../../escape"
        (data / "snapshots" / "h.json").write_text(json.dumps(doc))
        r = _run(real_cli, [f"--data-dir={data}", *ref_set_argv(data, "h.json")])
        assert r.returncode != 0
        assert not (tmp_path / "escape.json").exists()
        assert not list((data / "references").glob("*")) if (data / "references").exists() else True

    def test_ref_set_replaces_and_says_so_on_stdout(self, real_cli, tmp_path):
        data = tmp_path / "data"
        (data / "snapshots").mkdir(parents=True)
        base = json.loads((_VALIDITY / "snapshots" / "ok.json").read_text())
        for name, label in (("a.json", "first"), ("b.json", "second")):
            (data / "snapshots" / name).write_text(json.dumps(dict(base, label=label)))
        first = _run(real_cli, [f"--data-dir={data}", *ref_set_argv(data, "a.json")])
        assert first.returncode == 0, first.stderr
        second = _run(real_cli, [f"--data-dir={data}", *ref_set_argv(data, "b.json")])
        assert second.returncode == 0, second.stderr
        assert "replaced the previous reference" in second.stdout
        listing = parse_listing(_run(real_cli, [f"--data-dir={data}", *ls_argv()]).stdout)
        assert listing_ref_marks(listing) == {("Z 9", "0000003023668"): ["b.json"]}
