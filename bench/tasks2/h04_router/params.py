"""Route-parameter converters.

A pattern segment like <int:id> uses the "int" converter; a converter
either produces the typed value or signals that the segment does not
match this route.
"""

CONVERTERS = {
    "int": lambda raw: int(raw),
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
