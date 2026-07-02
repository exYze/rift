"""In-memory item store."""

_ITEMS = {
    "hardware": [("nut", 900), ("bolt", 250), ("washer", 40), ("screw", 610)],
    "produce": [("apple", 30), ("pear", 12), ("plum", 55)],
    "stationery": [("pen", 120), ("pad", 45)],
}


def load_items(source):
    """Return the (name, qty) rows for `source`, sorted by name.

    Unknown sources yield an empty list.
    """
    return sorted(_ITEMS.get(source, []))


def sources():
    """All known source names, sorted."""
    return sorted(_ITEMS)
