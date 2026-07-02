"""In-memory item store."""

_ITEMS = {
    "hardware": [("nut", 900), ("bolt", 250), ("washer", 40), ("screw", 610)],
    "produce": [("apple", 30), ("pear", 12), ("plum", 55)],
    "stationery": [("pen", 120), ("pad", 45)],
}


def fetch_items(source, limit=None):
    """Return the (name, qty) rows for `source`, sorted by name.

    When `limit` is given, at most that many rows are returned (the cap
    is applied after sorting). Unknown sources yield an empty list.
    """
    rows = sorted(_ITEMS.get(source, []))
    if limit is not None:
        rows = rows[:limit]
    return rows


def sources():
    """All known source names, sorted."""
    return sorted(_ITEMS)
