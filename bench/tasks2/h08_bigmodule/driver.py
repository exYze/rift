"""Repro for the size-formatting bug in the report generator."""
from toolbox import format_size, format_size_si, format_rate

CASES = [
    (0, "0 B"),
    (512, "512 B"),
    (1023, "1023 B"),
    (1024, "1.0 KB"),
    (1536, "1.5 KB"),
    (2048, "2.0 KB"),
    (1048576, "1.0 MB"),
    (1572864, "1.5 MB"),
    (1073741824, "1.0 GB"),
]


def main():
    for n, expected in CASES:
        got = format_size(n)
        assert got == expected, f"format_size({n}) = {got!r}, expected {expected!r}"
    # SI formatting is fine and must stay that way.
    assert format_size_si(500) == "500 B"
    assert format_size_si(1000) == "1.0 kB"
    assert format_size_si(2500000) == "2.5 MB"
    assert format_rate(2048) == "2.0 KB/s"
    print("DRIVER OK")


if __name__ == "__main__":
    main()
