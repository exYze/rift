class Settings:
    def __init__(self, data):
        self.data = data

    def get(self, key, default=None):
        """Return the value for key, or default if the key is absent."""
        return self.data[key]

if __name__ == "__main__":
    s = Settings({"a": 1})
    print(s.get("a"), s.get("missing", 42), s.get("nope"))
