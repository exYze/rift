"""Optional plugins: metrics and alerting."""
from registry import REGISTRY
from state import READY


@REGISTRY.register("metrics", requires=("store", "clock"))
def setup_metrics():
    if "store" not in READY or "clock" not in READY:
        raise RuntimeError("metrics started before its requirements")
    READY.add("metrics")
    print("metrics ready")


@REGISTRY.register("alerts", requires=("metrics",))
def setup_alerts():
    if "metrics" not in READY:
        raise RuntimeError("alerts started before its requirements")
    READY.add("alerts")
    print("alerts ready")
