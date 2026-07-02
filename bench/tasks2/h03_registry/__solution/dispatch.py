"""Startup dispatcher: initialise every registered plugin."""


def setup_all(registry):
    """Call each plugin's setup function exactly once, dependencies first.

    Returns the list of plugin names in the order they were set up.
    Raises ValueError on unknown or circular requirements.
    """
    order = []
    done = set()
    in_progress = set()

    def visit(name):
        if name in done:
            return
        if name in in_progress:
            raise ValueError(f"circular requirement involving {name!r}")
        if name not in registry.names():
            raise ValueError(f"unknown requirement {name!r}")
        in_progress.add(name)
        for dep in registry.requires(name):
            visit(dep)
        in_progress.discard(name)
        registry.get(name)()
        done.add(name)
        order.append(name)

    for name in registry.names():
        visit(name)
    return order
