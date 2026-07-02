"""Room-booking helpers built on the interval primitives."""
from intervals import merge, overlaps


def free_slots(day_start, day_end, busy):
    """Free intervals within [day_start, day_end) around the busy ones.

    `busy` is a list of half-open intervals in any order; they may
    overlap each other and may poke out past the day boundaries.
    """
    slots = []
    cursor = day_start
    for start, end in merge(busy):
        if start > cursor:
            slots.append((cursor, min(start, day_end)))
        cursor = max(cursor, end)
    return slots


def can_book(day_start, day_end, busy, want):
    """Can the half-open interval `want` be booked without clashing?"""
    if want[0] < day_start or want[1] > day_end:
        return False
    return not any(overlaps(want, b) for b in busy)
