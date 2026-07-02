"""Numeric rollups over per-document measurements."""
from util import sliding


def moving_sum(values, n):
    """Sums over every length-n window of values."""
    return [sum(w) for w in sliding(values, n)]


def moving_avg(values, n):
    """Averages over every length-n window of values."""
    return [s / n for s in moving_sum(values, n)]


def total(values):
    return sum(values)
