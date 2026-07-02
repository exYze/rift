"""A tiny path router with typed parameters, e.g. /users/<int:id>."""
from params import convert


class Router:
    def __init__(self):
        self._routes = []

    def add(self, pattern, handler):
        """Register a handler for a slash-separated pattern."""
        self._routes.append((pattern.strip("/").split("/"), handler))

    def match(self, path):
        """Return (handler, params) for the first matching route,
        or (None, None) when nothing matches."""
        segments = path.strip("/").split("/")
        for pattern, handler in self._routes:
            params = self._try_match(pattern, segments)
            if params is not None:
                return handler, params
        return None, None

    def _try_match(self, pattern, segments):
        if len(pattern) != len(segments):
            return None
        params = {}
        for pat_seg, seg in zip(pattern, segments):
            if pat_seg.startswith("<") and pat_seg.endswith(">"):
                kind, _, name = pat_seg[1:-1].partition(":")
                if not name:
                    kind, name = "str", kind
                ok, value = convert(kind, seg)
                if not ok:
                    return None
                params[name] = value
            elif pat_seg != seg:
                return None
        return params
