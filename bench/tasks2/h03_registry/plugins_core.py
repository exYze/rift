"""Core plugins: storage and clock."""
from registry import REGISTRY
from state import READY


@REGISTRY.register("store")
def setup_store():
    READY.add("store")
    print("store ready")


@REGISTRY.register("clock")
def setup_clock():
    READY.add("clock")
    print("clock ready")
