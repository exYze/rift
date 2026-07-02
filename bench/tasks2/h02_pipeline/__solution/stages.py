"""Token-level pipeline stages."""
from util import sliding


def tokenize(text):
    """Lowercase whitespace tokenizer."""
    return text.lower().split()


def bigrams(tokens):
    """All adjacent token pairs of a document, in order."""
    return [tuple(w) for w in sliding(tokens, 2)]


def vocabulary(tokens):
    """Sorted unique tokens."""
    return sorted(set(tokens))
