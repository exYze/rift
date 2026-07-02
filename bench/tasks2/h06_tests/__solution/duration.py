"""Parse and format human-friendly durations like "1h30m"."""
import re

UNITS = {"s": 1, "m": 60, "h": 3600, "d": 86400}
ORDER = (("d", 86400), ("h", 3600), ("m", 60), ("s", 1))

_SPEC = re.compile(r"(\d+)([smhd])")


def parse_duration(text):
    """Parse a duration spec into whole seconds.

    A spec is one or more <number><unit> tokens run together, e.g.
    "45s", "90m", "1h30m", "2d4h". Anything else raises ValueError.
    """
    text = text.strip()
    if not re.fullmatch(r"(?:\d+[smhd])+", text):
        raise ValueError(f"bad duration spec: {text!r}")
    return sum(int(n) * UNITS[u] for n, u in _SPEC.findall(text))


def format_duration(seconds):
    """Render whole seconds compactly, largest unit first: 3661 -> "1h1m1s"."""
    if seconds < 0:
        raise ValueError("negative duration")
    parts = []
    for unit, size in ORDER:
        if seconds >= size:
            n, seconds = divmod(seconds, size)
            parts.append(f"{n}{unit}")
    return "".join(parts) or "0s"
