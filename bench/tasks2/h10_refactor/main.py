"""Driver for the new store API. Do not change this file — make the
rest of the codebase match it."""
from store import fetch_items
from inventory import total_qty, low_stock
from reports import summary, first_rows, full_report
import cli


def main():
    assert fetch_items("hardware", limit=2) == [("bolt", 250), ("nut", 900)], \
        fetch_items("hardware", limit=2)
    assert fetch_items("hardware") == [
        ("bolt", 250), ("nut", 900), ("screw", 610), ("washer", 40)]
    assert fetch_items("produce", limit=1) == [("apple", 30)]
    assert fetch_items("nope") == []
    assert fetch_items("nope", limit=3) == []

    assert total_qty("produce") == 97
    assert low_stock("hardware", 100) == ["washer"]
    assert summary("hardware") == "hardware: 4 kinds, 1800 units"
    assert first_rows("hardware", 2) == [("bolt", 250), ("nut", 900)]
    assert full_report(["produce", "stationery"]) == (
        "produce: 3 kinds, 97 units\nstationery: 2 kinds, 165 units")
    assert cli.run(["produce"]) == 1

    print("REFACTOR OK")


if __name__ == "__main__":
    main()
