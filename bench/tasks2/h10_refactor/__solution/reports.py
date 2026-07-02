"""Human-readable inventory reports."""
import store
from inventory import total_qty


def summary(source):
    """One-line summary for a source."""
    rows = store.fetch_items(source)
    return f"{source}: {len(rows)} kinds, {total_qty(source)} units"


def first_rows(source, n):
    """The first n rows of a source (by the store's name ordering)."""
    return store.fetch_items(source, limit=n)


def full_report(sources):
    """Multi-line report over several sources."""
    return "\n".join(summary(s) for s in sources)
