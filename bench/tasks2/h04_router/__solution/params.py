"""Route-parameter converters.

A pattern segment like <int:id> uses the "int" converter; a converter
either produces the typed value or signals that the segment does not
match this route.
"""


def _to_int(raw):
    # Route ints are plain non-negative decimal digits only: no sign,
    # no whitespace, no floats.
    if not raw.isdigit():
        raise ValueError(f"not a route int: {raw!r}")
    return int(raw)


CONVERTERS = {
    "int": _to_int,
    "str": lambda raw: raw,
}


def convert(kind, raw):
    """Try to convert one path segment.

    Returns (True, value) when the segment matches the converter,
    (False, None) when it does not.
    """
    try:
        return True, CONVERTERS[kind](raw)
    except (KeyError, ValueError):
        return False, None
