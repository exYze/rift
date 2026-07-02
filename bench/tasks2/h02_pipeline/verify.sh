#!/usr/bin/env bash
set -e
python3 run.py > /dev/null
python3 - <<'PYEOF'
from util import sliding
from stages import bigrams
from stats import moving_sum, moving_avg
from pipeline import analyze

# The shared window helper must include the final window.
assert sliding([1, 2, 3, 4], 2) == [[1, 2], [2, 3], [3, 4]], sliding([1, 2, 3, 4], 2)
assert sliding([1, 2], 2) == [[1, 2]]
assert sliding([1], 2) == []
assert sliding([], 3) == []
assert sliding([5, 6, 7], 3) == [[5, 6, 7]]

# Bigrams: every adjacent pair exactly once, short docs have none.
assert bigrams(["a", "b", "c"]) == [("a", "b"), ("b", "c")], bigrams(["a", "b", "c"])
assert bigrams(["a", "b"]) == [("a", "b")]
assert bigrams(["solo"]) == []
assert bigrams([]) == []

# Moving sums cover the full series.
assert moving_sum([1, 2, 3, 4], 2) == [3, 5, 7], moving_sum([1, 2, 3, 4], 2)
assert moving_sum([6, 3, 1, 3], 2) == [9, 4, 4]
assert moving_avg([2, 4, 6], 2) == [3.0, 5.0]

# End to end.
r = analyze(["the cat sat on the mat", "the dog sat", "hello", "the cat ran"])
assert r["lengths"] == [6, 3, 1, 3]
assert r["length_sums"] == [9, 4, 4], r["length_sums"]
assert r["bigram_counts"][("the", "cat")] == 2
assert r["bigram_counts"][("dog", "sat")] == 1
assert r["bigram_counts"][("cat", "ran")] == 1
assert r["bigram_counts"][("the", "mat")] == 1
assert sum(r["bigram_counts"].values()) == 5 + 2 + 0 + 2
print("VERIFY OK")
PYEOF
