"""Inventory math on top of the store."""
from store import fetch_items


def total_qty(source):
    """Total units on hand for a source."""
    return sum(qty for _, qty in fetch_items(source))


def kinds(source):
    """How many distinct item names a source carries."""
    return len(fetch_items(source))


def low_stock(source, threshold):
    """Names with quantity strictly below `threshold`, sorted."""
    return sorted(name for name, qty in fetch_items(source) if qty < threshold)
