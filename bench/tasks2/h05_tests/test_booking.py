"""Tests for the interval and booking helpers. Do not modify."""
import unittest

from intervals import overlaps, merge, length
from booking import free_slots, can_book


class TestOverlaps(unittest.TestCase):
    def test_touching_intervals_do_not_overlap(self):
        # Half-open: [1,3) and [3,5) share no point.
        self.assertFalse(overlaps((1, 3), (3, 5)))
        self.assertFalse(overlaps((3, 5), (1, 3)))

    def test_real_overlap(self):
        self.assertTrue(overlaps((1, 4), (3, 5)))
        self.assertTrue(overlaps((3, 5), (1, 4)))

    def test_containment(self):
        self.assertTrue(overlaps((1, 10), (4, 5)))

    def test_disjoint(self):
        self.assertFalse(overlaps((1, 2), (5, 6)))


class TestMerge(unittest.TestCase):
    def test_unsorted_input(self):
        self.assertEqual(merge([(5, 7), (1, 3), (3, 5)]), [(1, 7)])

    def test_adjacent_merge(self):
        self.assertEqual(merge([(1, 3), (3, 5)]), [(1, 5)])

    def test_disjoint_sorted_output(self):
        self.assertEqual(merge([(8, 9), (1, 2)]), [(1, 2), (8, 9)])

    def test_contained(self):
        self.assertEqual(merge([(1, 10), (2, 3)]), [(1, 10)])

    def test_empty(self):
        self.assertEqual(merge([]), [])


class TestFreeSlots(unittest.TestCase):
    def test_gap_after_last_meeting(self):
        self.assertEqual(free_slots(9, 17, [(10, 11), (13, 14)]),
                         [(9, 10), (11, 13), (14, 17)])

    def test_no_meetings_whole_day_free(self):
        self.assertEqual(free_slots(9, 17, []), [(9, 17)])

    def test_fully_booked(self):
        self.assertEqual(free_slots(9, 17, [(9, 17)]), [])

    def test_unsorted_busy(self):
        self.assertEqual(free_slots(9, 17, [(13, 14), (10, 11)]),
                         [(9, 10), (11, 13), (14, 17)])

    def test_meeting_past_day_end(self):
        self.assertEqual(free_slots(9, 17, [(16, 19)]), [(9, 16)])


class TestCanBook(unittest.TestCase):
    def test_back_to_back_is_fine(self):
        self.assertTrue(can_book(9, 17, [(10, 11)], (11, 12)))
        self.assertTrue(can_book(9, 17, [(10, 11)], (9, 10)))

    def test_clash(self):
        self.assertFalse(can_book(9, 17, [(10, 12)], (11, 13)))

    def test_outside_day(self):
        self.assertFalse(can_book(9, 17, [], (8, 10)))

    def test_length(self):
        self.assertEqual(length((3, 8)), 5)


if __name__ == "__main__":
    unittest.main()
