import json
from pathlib import Path

import pytest
from fleet_lib import strip_sdk_prefix, accept_zip_entry, parse_fw_filename, fmt_cap_value, model_slug, parse_propcode

_FIXTURE = Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "capability_value_shapes.json"


# ── model_slug ─────────────────────────────────────────────────────────────────
# Parity with the Rust side's firmware::model_slug() — both must produce
# identical filenames from the same model string, or reference/archive
# lookups silently fail to find each other's files.

class TestModelSlug:
    def test_replaces_spaces(self):
        assert model_slug("Z 9") == "Z_9"
        assert model_slug("Z 6III") == "Z_6III"

    def test_no_spaces_unchanged(self):
        assert model_slug("Z6_3") == "Z6_3"


# ── strip_sdk_prefix ──────────────────────────────────────────────────────────

class TestStripSdkPrefix:
    def test_strips_diagnostic_line(self):
        out = 'InitializeSDK Execution duration: 204\n{"cameras":[]}'
        assert strip_sdk_prefix(out) == '{"cameras":[]}'

    def test_pure_json_unchanged(self):
        out = '{"cameras":[{"serial":"123"}]}'
        assert strip_sdk_prefix(out) == out

    def test_multiple_diagnostic_lines(self):
        out = "line one\nline two\n{\"k\":1}"
        assert strip_sdk_prefix(out) == '{"k":1}'

    def test_brace_in_prefix_ignored(self):
        # A '{' INSIDE a diagnostic line must not be treated as JSON start.
        # Only a '{' that begins a line (pos=0 or after '\n') is accepted.
        out = 'SDK {debug} info\n{"ok":true}'
        assert strip_sdk_prefix(out) == '{"ok":true}'

    def test_no_json_raises(self):
        with pytest.raises(ValueError):
            strip_sdk_prefix("no json here")

    def test_empty_string_raises(self):
        with pytest.raises(ValueError):
            strip_sdk_prefix("")


# ── accept_zip_entry ──────────────────────────────────────────────────────────

class TestAcceptZipEntry:
    # accepted cases
    def test_snapshot_json(self):
        assert accept_zip_entry("snapshots/foo.json") is True

    def test_reference_json(self):
        assert accept_zip_entry("references/bar.json") is True

    def test_firmware_bin(self):
        # Nested layout: firmware/{model_slug}/{version}/firmware.bin
        assert accept_zip_entry("firmware/Z_9/5.31/firmware.bin") is True

    def test_firmware_metadata_json(self):
        assert accept_zip_entry("firmware/Z_9/5.31/metadata.json") is True

    def test_firmware_z6iii(self):
        assert accept_zip_entry("firmware/Z6_3/2.00/firmware.bin") is True

    # wrong extension / wrong filename
    def test_snapshot_bin_rejected(self):
        assert accept_zip_entry("snapshots/foo.bin") is False

    def test_firmware_arbitrary_json_rejected(self):
        # Only metadata.json (exact filename) is accepted, not arbitrary .json
        assert accept_zip_entry("firmware/Z_9/5.31/other.json") is False

    def test_firmware_flat_bin_rejected(self):
        # Old flat layout (2-part) is no longer accepted
        assert accept_zip_entry("firmware/Z_9_0531.bin") is False

    def test_reference_bin_rejected(self):
        assert accept_zip_entry("references/bar.bin") is False

    # unknown folder
    def test_unknown_folder_rejected(self):
        assert accept_zip_entry("other/foo.json") is False

    # wrong depth
    def test_directory_entry_rejected(self):
        # zip directory entries have a trailing slash → single Path part
        assert accept_zip_entry("snapshots/") is False

    def test_too_deep_rejected(self):
        assert accept_zip_entry("snapshots/subdir/foo.json") is False

    def test_firmware_too_shallow_rejected(self):
        # 3-part firmware path (missing version level) is rejected
        assert accept_zip_entry("firmware/Z_9/firmware.bin") is False

    def test_firmware_too_deep_rejected(self):
        # 5-part firmware path is rejected
        assert accept_zip_entry("firmware/Z_9/5.31/extra/firmware.bin") is False

    # path-traversal attacks
    def test_dotdot_in_firmware_path_rejected(self):
        # 4-part path with ".." as middle component — must be rejected
        assert accept_zip_entry("firmware/../snapshots/metadata.json") is False

    def test_dotdot_in_snapshots_path_rejected(self):
        assert accept_zip_entry("snapshots/../etc/passwd") is False

    def test_dotdot_as_slug_component_rejected(self):
        assert accept_zip_entry("firmware/../Z_9/5.31/firmware.bin") is False

    def test_bare_filename_rejected(self):
        assert accept_zip_entry("foo.json") is False

    # path traversal
    def test_path_traversal_rejected(self):
        assert accept_zip_entry("../etc/passwd") is False

    def test_absolute_path_rejected(self):
        assert accept_zip_entry("/snapshots/foo.json") is False

    def test_curdir_component_rejected(self):
        # Regression: Path(name).parts silently dropped "." components,
        # diverging from gui/src/main.rs's Rust accept_zip_entry (which
        # rejects Component::CurDir) even though both sides are commented
        # "must stay in sync."
        assert accept_zip_entry("snapshots/./foo.json") is False


# ── parse_fw_filename ─────────────────────────────────────────────────────────

class TestParseFwFilename:
    # known fleet bodies
    def test_z9_531(self):
        assert parse_fw_filename("Z_9_0531.bin") == ("Z_9", "5.31")

    def test_z9_532(self):
        assert parse_fw_filename("Z_9_0532.bin") == ("Z_9", "5.32")

    def test_z6iii_200(self):
        assert parse_fw_filename("Z6_3_0200.bin") == ("Z6_3", "2.00")

    def test_z6ii_170(self):
        assert parse_fw_filename("Z6_2_0170.bin") == ("Z6_2", "1.70")

    def test_z30_120(self):
        assert parse_fw_filename("Z_30_0120.bin") == ("Z_30", "1.20")

    def test_z8_310(self):
        assert parse_fw_filename("Z_8_0310.bin") == ("Z_8", "3.10")

    # version boundary: major padded to two digits
    def test_major_zero_padded(self):
        assert parse_fw_filename("Z_9_0100.bin") == ("Z_9", "1.00")

    def test_major_two_digits(self):
        assert parse_fw_filename("Z_9_1020.bin") == ("Z_9", "10.20")

    # non-matching patterns — fall back to (stem, "")
    def test_non_digit_suffix(self):
        model, ver = parse_fw_filename("Z_9_v531.bin")
        assert ver == ""

    def test_three_digit_version(self):
        model, ver = parse_fw_filename("Z_9_531.bin")
        assert ver == ""

    def test_five_digit_version(self):
        model, ver = parse_fw_filename("Z_9_05310.bin")
        assert ver == ""

    def test_no_underscore_version(self):
        model, ver = parse_fw_filename("firmware.bin")
        assert ver == ""

    def test_extension_not_included_in_model(self):
        model, _ = parse_fw_filename("Z_9_0531.bin")
        assert not model.endswith(".bin")


# ── parse_propcode ────────────────────────────────────────────────────────────
# Must accept the same forms as the Rust CLI's own parse_propcode (main.rs) —
# both parse the same user-typed string, one from a GUI entry, one from argv.

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

    # Regression tests for two confirmed divergences from the Rust CLI's
    # own parse_propcode (src/ptp_usb.rs): this module's prior
    # implementation used Python's int(), which tolerates both forms below
    # even though u16::from_str_radix/str::parse do not. Pinned here (and
    # mirrored on the Rust side) so a future edit can't silently reopen
    # either gap between the two languages' "identical" parsers.

    def test_internal_whitespace_after_hex_prefix_raises(self):
        with pytest.raises(ValueError):
            parse_propcode("0x 10")

    def test_underscore_digit_separators_raise(self):
        with pytest.raises(ValueError):
            parse_propcode("0xD0_53")
        with pytest.raises(ValueError):
            parse_propcode("53_331")

    def test_leading_plus_accepted(self):
        # Matches Rust: u16::from_str_radix/FromStr both accept a leading
        # '+', unlike the whitespace/underscore forms above.
        assert parse_propcode("+53331") == 53331
        assert parse_propcode("0x+D053") == 0xD053


# decode_packed_strings and its tests were removed: the function was never
# called from fleet_gui.py's runtime path (elem_type=7 values arrive
# already decoded from Rust's decode_enum_values), so its tests were
# pinning a decode path that isn't wired into production.

# ── fmt_cap_value ─────────────────────────────────────────────────────────────

class TestFmtCapValueFixtureCrossCheck:
    """Loads tests/fixtures/capability_value_shapes.json — the same file
    src/sdk.rs's tests assert is exactly what decode_value()/
    decode_enum_values() produce for each branch — and runs fmt_cap_value
    against every entry. Closes the gap where this Python parser was
    hand-matched to the Rust JSON shape with nothing to catch drift between
    them; edit both test files together with the fixture.
    """

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
        assert fmt_cap_value(shapes["range"]) == "1.0"


class TestFmtCapValue:
    def test_unsupported_enum_type_fallback_does_not_crash(self):
        # Regression: decode_enum_values (Rust) falls back to a JSON
        # object — {"_unsupported_enum_type": N} — for an array-element
        # type it doesn't decode. This used to raise KeyError (dict[0] on
        # a dict whose only key is a string) instead of rendering
        # gracefully.
        v = {"elem_type": 99, "value_index": 0, "elem_count": 1,
             "elem_bytes": 4, "default_index": 0,
             "values": {"_unsupported_enum_type": 99}}
        result = fmt_cap_value(v)
        assert isinstance(result, str)
        assert "_unsupported_enum_type" in result

    def test_invalid_physical_bytes_fallback_does_not_crash(self):
        v = {"elem_type": 7, "value_index": 0, "elem_count": 2,
             "elem_bytes": -1, "default_index": 0,
             "values": {"_invalid_physical_bytes": -1}}
        result = fmt_cap_value(v)
        assert isinstance(result, str)
        assert "_invalid_physical_bytes" in result


    def test_scalar_int(self):
        assert fmt_cap_value(100) == "100"

    def test_scalar_float(self):
        assert fmt_cap_value(5.6) == "5.6"

    def test_bool_true(self):
        assert fmt_cap_value(True) == "Yes"

    def test_bool_false(self):
        assert fmt_cap_value(False) == "No"

    def test_string_passthrough(self):
        assert fmt_cap_value("USB") == "USB"

    def test_range_dict(self):
        v = {"lower": -5.0, "upper": 5.0, "steps": 31, "value": 0.0,
             "value_index": 15, "default": 0.0, "default_index": 15}
        assert fmt_cap_value(v) == "0.0"

    def test_range_dict_nonzero(self):
        v = {"lower": -5.0, "upper": 5.0, "steps": 31, "value": 1.0,
             "value_index": 18, "default": 0.0, "default_index": 15}
        assert fmt_cap_value(v) == "1.0"

    def test_packed_string_enum(self):
        # Rust decodes elem_type=7 into already-decoded string labels.
        # fmt_cap_value must index raw_vals[value_index] directly.
        # CompressionLevel: value_index=3 → "RAW"
        labels = ["JPEG Basic", "JPEG Normal", "JPEG Fine", "RAW", "RAW+Basic", "RAW+Fine"]
        v = {"elem_type": 7, "value_index": 3, "elem_count": 6,
             "elem_bytes": 1, "default_index": 0, "values": labels}
        assert fmt_cap_value(v) == "RAW"

    def test_packed_string_enum_aperture(self):
        # Aperture value_index=0 → "f/4" (already decoded string from Rust)
        labels = ["f/4", "f/4.5", "f/5.6"]
        v = {"elem_type": 7, "value_index": 0, "elem_count": 3,
             "elem_bytes": 1, "default_index": 0, "values": labels}
        assert fmt_cap_value(v) == "f/4"

    def test_integer_enum(self):
        # ExposureMode: values=[0,1,2,3], value_index=2 → "2"
        v = {"elem_type": 2, "value_index": 2, "elem_count": 4,
             "elem_bytes": 4, "default_index": 0, "values": [0, 1, 2, 3]}
        assert fmt_cap_value(v) == "2"

    def test_out_of_bounds_index(self):
        v = {"elem_type": 7, "value_index": 99, "values": ["Auto"], "elem_bytes": 1,
             "elem_count": 1, "default_index": 0}
        result = fmt_cap_value(v)
        assert "99" in result

    # ── list rendering boundary (6 vs 7 items) ────────────────────────────

    def test_list_empty(self):
        assert fmt_cap_value([]) == "[]"

    def test_list_exactly_six(self):
        # Six items must render fully (no truncation).
        result = fmt_cap_value([1, 2, 3, 4, 5, 6])
        assert "…" not in result
        assert result == "[1, 2, 3, 4, 5, 6]"

    def test_list_exactly_seven(self):
        # Seven items must use the truncated "first, … N items" form.
        result = fmt_cap_value([1, 2, 3, 4, 5, 6, 7])
        assert "…" in result
        assert "7 items" in result
