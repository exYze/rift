"""Handler middleware: logging plus a last-resort error trap."""

LOG = []


def wrap(handler):
    """Wrap a handler: record the call, convert crashes into a 500."""
    def wrapped(**params):
        LOG.append((handler.__name__, dict(params)))
        try:
            return handler(**params)
        except Exception:
            return 500, "internal error"
    return wrapped
