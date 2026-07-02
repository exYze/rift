"""Half-open interval helpers. An interval is a (start, end) tuple
meaning [start, end): start is included, end is not."""


def overlaps(a, b):
    """Do two half-open intervals share any point?"""
    return a[0] <= b[1] and b[0] <= a[1]


def merge(intervals):
    """Merge overlapping or adjacent half-open intervals.

    Returns a sorted list of disjoint intervals covering the same points.
    """
    merged = []
    for start, end in intervals:
        if merged and start <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged


def length(interval):
    """Total length of one interval."""
    return max(0, interval[1] - interval[0])
