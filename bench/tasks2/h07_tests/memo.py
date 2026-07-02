"""Memoisation decorator backed by the LRU cache."""
import functools

from lru import LRUCache

_SENTINEL = object()


def memoize(capacity):
    """Cache a function's results by positional arguments, LRU-bounded."""
    def deco(fn):
        cache = LRUCache(capacity)

        @functools.wraps(fn)
        def wrapper(*args):
            hit = cache.get(args, _SENTINEL)
            if hit is not _SENTINEL:
                return hit
            result = fn(*args)
            cache.put(args, result)
            return result

        wrapper.cache = cache
        return wrapper
    return deco
