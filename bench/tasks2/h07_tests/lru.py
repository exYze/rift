"""A small least-recently-used cache with hit/miss counters."""
from collections import OrderedDict


class LRUCache:
    """Bounded mapping that evicts the least recently *used* entry.

    Both get() and put() count as a use of the key.
    """

    def __init__(self, capacity):
        if capacity <= 0:
            raise ValueError("capacity must be positive")
        self.capacity = capacity
        self._data = OrderedDict()
        self.hits = 0
        self.misses = 0

    def get(self, key, default=None):
        """Return the cached value, or `default` on a miss."""
        if key in self._data:
            self.hits += 1
            return self._data[key]
        self.misses += 1
        return default

    def put(self, key, value):
        """Insert or update a key, evicting the LRU entry if over capacity."""
        self._data[key] = value
        if len(self._data) > self.capacity:
            self._data.popitem(last=False)

    def __contains__(self, key):
        return key in self._data

    def __len__(self):
        return len(self._data)

    def keys(self):
        """Keys ordered least-recently-used first."""
        return list(self._data)
