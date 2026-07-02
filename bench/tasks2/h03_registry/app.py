"""Boot the app: import plugin modules (which registers them), then
initialise everything."""
import plugins_extra  # noqa: F401  (registers metrics, alerts)
import plugins_core   # noqa: F401  (registers store, clock)

from registry import REGISTRY
from dispatch import setup_all


def main():
    order = setup_all(REGISTRY)
    print("boot ok:", ",".join(order))


if __name__ == "__main__":
    main()
