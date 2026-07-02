"""Tests for the LRU cache and memoize decorator. Do not modify."""
import unittest

from lru import LRUCache
from memo import memoize


class TestLRUBasics(unittest.TestCase):
    def test_put_get(self):
        c = LRUCache(2)
        c.put("a", 1)
        self.assertEqual(c.get("a"), 1)
        self.assertEqual(c.get("missing", 42), 42)

    def test_eviction_order_of_inserts(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.put("b", 2)
        c.put("c", 3)
        self.assertNotIn("a", c)
        self.assertIn("b", c)
        self.assertIn("c", c)

    def test_bad_capacity(self):
        with self.assertRaises(ValueError):
            LRUCache(0)


class TestRecency(unittest.TestCase):
    def test_get_refreshes_recency(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.put("b", 2)
        c.get("a")           # "a" is now most recently used
        c.put("c", 3)        # so "b" must be the one evicted
        self.assertIn("a", c)
        self.assertNotIn("b", c)
        self.assertIn("c", c)

    def test_update_refreshes_recency(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.put("b", 2)
        c.put("a", 10)       # update: "a" is most recently used again
        c.put("c", 3)        # so "b" must be the one evicted
        self.assertEqual(c.get("a"), 10)
        self.assertNotIn("b", c)
        self.assertIn("c", c)

    def test_keys_lru_first(self):
        c = LRUCache(3)
        c.put("a", 1)
        c.put("b", 2)
        c.put("c", 3)
        c.get("a")
        self.assertEqual(c.keys(), ["b", "c", "a"])

    def test_capacity_one(self):
        c = LRUCache(1)
        c.put("a", 1)
        c.put("a", 2)
        self.assertEqual(len(c), 1)
        self.assertEqual(c.get("a"), 2)
        c.put("b", 3)
        self.assertNotIn("a", c)


class TestCounters(unittest.TestCase):
    def test_hits_and_misses(self):
        c = LRUCache(2)
        c.put("a", 1)
        c.get("a")
        c.get("a")
        c.get("zzz")
        self.assertEqual(c.hits, 2)
        self.assertEqual(c.misses, 1)


class TestMemoize(unittest.TestCase):
    def test_repeat_calls_cached(self):
        calls = []

        @memoize(capacity=4)
        def square(x):
            calls.append(x)
            return x * x

        self.assertEqual(square(3), 9)
        self.assertEqual(square(3), 9)
        self.assertEqual(calls, [3])

    def test_hot_key_survives_eviction(self):
        calls = []

        @memoize(capacity=2)
        def square(x):
            calls.append(x)
            return x * x

        square(1)            # cache: 1
        square(2)            # cache: 1, 2
        square(1)            # hit — 1 becomes most recent
        square(3)            # evicts 2, NOT the hot key 1
        square(1)            # must still be a hit
        self.assertEqual(calls, [1, 2, 3])


if __name__ == "__main__":
    unittest.main()
