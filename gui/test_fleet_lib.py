import pytest
from fleet_lib import strip_sdk_prefix, accept_zip_entry, parse_fw_filename, decode_packed_strings, fmt_cap_value


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
        # A '{' inside a diagnostic line must not confuse the parser — we want
        # the FIRST '{', which the SDK diagnostic lines don't contain.
        out = 'Duration: 42\n{"ok":true}'
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


# ── decode_packed_strings ─────────────────────────────────────────────────────

class TestDecodePackedStrings:
    def test_basic(self):
        chars = ["R","A","W","", "J","P","E","G",""]
        assert decode_packed_strings(chars) == ["RAW", "JPEG"]

    def test_single_char_entries(self):
        # Aperture-style: single-digit labels
        chars = ["4","", "5","", "8",""]
        assert decode_packed_strings(chars) == ["4", "5", "8"]

    def test_multichar_entry(self):
        chars = ["J","P","E","G"," ","F","i","n","e",""]
        assert decode_packed_strings(chars) == ["JPEG Fine"]

    def test_trailing_no_terminator(self):
        # Last entry missing trailing ""
        chars = ["A","u","t","o","", "M","a","n","u","a","l"]
        assert decode_packed_strings(chars) == ["Auto", "Manual"]

    def test_empty_input(self):
        assert decode_packed_strings([]) == []

    def test_real_wb_fragment(self):
        # Fragment from WBMode: Auto / Incandescent
        chars = ["A","u","t","o","", "I","n","c","a","n","d","e","s","c","e","n","t",""]
        assert decode_packed_strings(chars) == ["Auto", "Incandescent"]


# ── fmt_cap_value ─────────────────────────────────────────────────────────────

class TestFmtCapValue:
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
        # CompressionLevel: value_index=6 → "RAW"
        chars = (["J","P","E","G"," ","B","a","s","i","c",""] +
                 ["J","P","E","G"," ","N","o","r","m","a","l",""] +
                 ["J","P","E","G"," ","F","i","n","e",""] +
                 ["R","A","W",""] +
                 ["R","A","W","+"," ","B","a","s","i","c",""] +
                 ["R","A","W","+"," ","F","i","n","e",""])
        v = {"elem_type": 7, "value_index": 3, "elem_count": 6,
             "elem_bytes": 1, "default_index": 0, "values": chars}
        assert fmt_cap_value(v) == "RAW"

    def test_packed_string_enum_aperture(self):
        # Aperture value_index=0 → "4" (f/4)
        chars = ["4","", "4",".","5","", "5",".", "6",""]
        v = {"elem_type": 7, "value_index": 0, "elem_count": 3,
             "elem_bytes": 1, "default_index": 0, "values": chars}
        assert fmt_cap_value(v) == "4"

    def test_integer_enum(self):
        # ExposureMode: values=[0,1,2,3], value_index=2 → "2"
        v = {"elem_type": 2, "value_index": 2, "elem_count": 4,
             "elem_bytes": 4, "default_index": 0, "values": [0, 1, 2, 3]}
        assert fmt_cap_value(v) == "2"

    def test_out_of_bounds_index(self):
        v = {"elem_type": 7, "value_index": 99, "values": ["A",""], "elem_bytes": 1,
             "elem_count": 1, "default_index": 0}
        result = fmt_cap_value(v)
        assert "99" in result
