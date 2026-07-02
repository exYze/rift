"""Plugin registry: plugins register a setup function under a name and
may declare which other plugins they require."""


class Registry:
    def __init__(self):
        self._plugins = {}  # name -> setup function
        self._requires = {}  # name -> tuple of required plugin names

    def register(self, name, requires=()):
        """Decorator: register `fn` as the setup function for `name`.

        `requires` lists plugin names that must be set up first.
        """
        def deco(fn):
            self._plugins[name] = fn
            self._requires[name] = tuple(requires)
            return fn
        return deco

    def get(self, name):
        """Return the setup function for `name`."""
        return self._plugins[name]

    def requires(self, name):
        """Return the names this plugin requires."""
        return self._requires.get(name, ())

    def names(self):
        """All registered plugin names, in registration order."""
        return list(self._plugins)


REGISTRY = Registry()
