"""Tiny CLI: print a stock line per requested source."""
import sys

from store import load_items
from inventory import total_qty


def run(argv):
    """Print "<source>: <rows> rows, <total> total" per source; returns
    the number of lines printed."""
    count = 0
    for source in argv:
        rows = load_items(source)
        print(f"{source}: {len(rows)} rows, {total_qty(source)} total")
        count += 1
    return count


if __name__ == "__main__":
    run(sys.argv[1:])
