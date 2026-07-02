"""Tests for the duration helpers. Do not modify."""
import unittest

from duration import parse_duration, format_duration
from timers import total_seconds, describe_total


class TestParse(unittest.TestCase):
    def test_single_units(self):
        self.assertEqual(parse_duration("45s"), 45)
        self.assertEqual(parse_duration("2d"), 172800)

    def test_overflowing_single_unit(self):
        self.assertEqual(parse_duration("90m"), 5400)

    def test_compound(self):
        self.assertEqual(parse_duration("1h30m"), 5400)
        self.assertEqual(parse_duration("2d4h"), 187200)
        self.assertEqual(parse_duration("1d2h3m4s"), 93784)

    def test_whitespace_tolerated_around_spec(self):
        self.assertEqual(parse_duration("  10m "), 600)

    def test_rejects_garbage(self):
        for bad in ("", "h", "3x", "90", "1h30", "-5m", "1.5h", "m5"):
            with self.assertRaises(ValueError, msg=bad):
                parse_duration(bad)


class TestFormat(unittest.TestCase):
    def test_zero(self):
        self.assertEqual(format_duration(0), "0s")

    def test_exact_units(self):
        self.assertEqual(format_duration(3600), "1h")
        self.assertEqual(format_duration(86400), "1d")

    def test_compound(self):
        self.assertEqual(format_duration(3661), "1h1m1s")
        self.assertEqual(format_duration(5400), "1h30m")

    def test_negative_rejected(self):
        with self.assertRaises(ValueError):
            format_duration(-1)


class TestTimers(unittest.TestCase):
    def test_total(self):
        self.assertEqual(total_seconds(["1h", "30m", "30m"]), 7200)
        self.assertEqual(total_seconds([]), 0)

    def test_describe(self):
        self.assertEqual(describe_total(["45m", "30m"]), "1h15m")
        self.assertEqual(describe_total([]), "0s")


if __name__ == "__main__":
    unittest.main()
