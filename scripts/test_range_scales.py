"""Tests for range_scales.py: python3 -m unittest scripts/test_range_scales.py"""
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import range_scales as rs  # noqa: E402


def rng(code, lower=0.0, upper=10.0, steps=11, default_index=0, value_index=3):
    return {"code": code, "lower": lower, "upper": upper, "steps": steps, "default_index": default_index, "value_index": value_index}


def report(model, **ranges):
    return {"model": model, "firmware": "1.00", "ranges": ranges}


class Classify(unittest.TestCase):
    def test_identical_scales_on_two_models_are_same_even_if_the_current_value_differs(self):
        a = report("A", R=rng(1, value_index=2))
        b = report("B", R=rng(1, value_index=9))
        same, differs, single = rs.classify([a, b])
        self.assertEqual((len(same), len(differs), len(single)), (1, 0, 0))
        self.assertEqual(same[0][2], ["A", "B"])

    def test_each_scale_field_on_its_own_makes_the_range_differ(self):
        base = rng(1)
        for field, other in [("lower", 1.0), ("upper", 11.0), ("steps", 12), ("default_index", 1)]:
            with self.subTest(field=field):
                b = report("B", R={**base, field: other})
                same, differs, _ = rs.classify([report("A", R=base), b])
                self.assertEqual((len(same), len(differs)), (0, 1))

    def test_one_float_unit_apart_differs(self):
        b = report("B", R=rng(1, lower=0.0 + sys.float_info.epsilon))
        _, differs, _ = rs.classify([report("A", R=rng(1)), b])
        self.assertEqual(len(differs), 1)

    def test_a_range_on_one_model_is_single_not_same(self):
        same, differs, single = rs.classify([report("A", R=rng(1)), report("B")])
        self.assertEqual((len(same), len(differs), len(single)), (0, 0, 1))

    def test_a_third_model_that_disagrees_splits_the_groups(self):
        a, b, c = report("A", R=rng(1)), report("B", R=rng(1)), report("C", R=rng(1, steps=5))
        _, differs, _ = rs.classify([a, b, c])
        groups = differs[0][2]
        self.assertEqual(sorted(sorted(m) for m in groups.values()), [["A", "B"], ["C"]])

    def test_two_reports_for_one_model_is_refused_not_merged(self):
        with self.assertRaises(ValueError):
            rs.classify([report("A", R=rng(1)), report("A", R=rng(1))])

    def test_no_reports_and_one_report_give_nothing_to_compare(self):
        self.assertEqual(rs.classify([]), ([], [], []))
        same, differs, single = rs.classify([report("A", R=rng(1))])
        self.assertEqual((same, differs, len(single)), ([], [], 1))

    def test_ranges_are_ordered_by_code(self):
        a = report("A", Z=rng(5), A=rng(2))
        b = report("B", Z=rng(5), A=rng(2))
        same, _, _ = rs.classify([a, b])
        self.assertEqual([c for _, c, _ in same], [2, 5])


class Dump(unittest.TestCase):
    def snapshot(self, props):
        return {"camera": {"model": "Z 9", "serial": "0000000000001", "firmware": "5.31"}, "captured_at": "t", "properties": props}

    def dump(self, snap):
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump(snap, f)
        try:
            return rs.dump(f.name)
        finally:
            os.unlink(f.name)

    def test_keeps_ranges_drops_everything_else_and_never_carries_the_serial(self):
        r = {"lower": 0.0, "upper": 4.0, "steps": 5, "default_index": 2, "value_index": 1, "value": 0.0, "default": 0.0}
        out = self.dump(self.snapshot({
            "kNkMAIDCapability_Range": {"code": 7, "value": r},
            "kNkMAIDCapability_Enum": {"code": 8, "value": {"values": [1], "value_index": 0, "default_index": 0}},
            "kNkMAIDCapability_Text": {"code": 9, "value": "hello"},
            "kNkMAIDCapability_Missing": {"code": 10, "value": {"lower": 0, "upper": 1}},
        }))
        self.assertEqual(list(out["ranges"]), ["Range"])
        self.assertEqual(out["ranges"]["Range"], {"code": 7, "lower": 0.0, "upper": 4.0, "steps": 5, "default_index": 2, "value_index": 1})
        self.assertNotIn("0000000000001", json.dumps(out))
        self.assertEqual((out["model"], out["firmware"], out["range_count"]), ("Z 9", "5.31", 1))

    def test_a_snapshot_with_no_ranges_gives_an_empty_report_not_an_error(self):
        out = self.dump(self.snapshot({}))
        self.assertEqual((out["ranges"], out["range_count"]), ({}, 0))


class CommittedReports(unittest.TestCase):
    """The reports in docs/range-scales/ carry the evidence for the verified table."""

    def test_they_are_one_per_model_with_no_serial_and_the_expected_split(self):
        d = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "docs", "range-scales")
        reports = [json.load(open(os.path.join(d, f))) for f in sorted(os.listdir(d)) if f.endswith(".json")]
        self.assertGreaterEqual(len(reports), 3)
        same, differs, _ = rs.classify(reports)
        self.assertIn("ShootingLimit", [n for n, _, _ in differs])
        self.assertTrue(all("serial" not in json.dumps(r) for r in reports))


if __name__ == "__main__":
    unittest.main()
