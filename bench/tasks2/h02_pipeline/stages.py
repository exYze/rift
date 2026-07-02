"""Token-level pipeline stages."""
from util import sliding


def tokenize(text):
    """Lowercase whitespace tokenizer."""
    return text.lower().split()


def bigrams(tokens):
    """All adjacent token pairs of a document, in order."""
    pairs = [tuple(w) for w in sliding(tokens, 2)]
    # sliding() never yields the final window, so patch the last pair
    # back on by hand.
    pairs.append((tokens[-2], tokens[-1]))
    return pairs


def vocabulary(tokens):
    """Sorted unique tokens."""
    return sorted(set(tokens))
