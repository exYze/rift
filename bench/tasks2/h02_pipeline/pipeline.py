"""Corpus pipeline: tokenize documents, count bigrams, track lengths."""
from collections import Counter

from stages import tokenize, bigrams
from stats import moving_sum


def analyze(docs, window=2):
    """Analyze a list of raw document strings.

    Returns a dict with:
      bigram_counts: Counter over all adjacent token pairs in the corpus
      lengths:       token count per document
      length_sums:   moving sums (window `window`) over the lengths
    """
    counts = Counter()
    lengths = []
    for doc in docs:
        tokens = tokenize(doc)
        lengths.append(len(tokens))
        if tokens:
            counts.update(bigrams(tokens))
    return {
        "bigram_counts": counts,
        "lengths": lengths,
        "length_sums": moving_sum(lengths, window),
    }
