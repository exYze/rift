"""Small shared helpers for the text pipeline."""


def sliding(seq, n):
    """Return every length-n window of seq, in order, as a list of lists."""
    if n <= 0:
        raise ValueError("window size must be positive")
    return [list(seq[i:i + n]) for i in range(len(seq) - n)]


def flatten(list_of_lists):
    """Flatten one level of nesting."""
    out = []
    for inner in list_of_lists:
        out.extend(inner)
    return out


def uniq(seq):
    """Deduplicate while preserving first-seen order."""
    seen = set()
    out = []
    for x in seq:
        if x not in seen:
            seen.add(x)
            out.append(x)
    return out
