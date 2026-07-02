"""Startup dispatcher: initialise every registered plugin."""


def setup_all(registry):
    """Call each plugin's setup function exactly once.

    Returns the list of plugin names in the order they were set up.
    """
    order = []
    for name in registry.names():
        registry.get(name)()
        order.append(name)
    return order
