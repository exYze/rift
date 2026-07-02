"""Timer helpers built on the duration parser."""
from duration import parse_duration, format_duration


def total_seconds(specs):
    """Sum a list of duration specs into seconds."""
    return sum(parse_duration(s) for s in specs)


def describe_total(specs):
    """Human-readable sum of duration specs."""
    return format_duration(total_seconds(specs))
