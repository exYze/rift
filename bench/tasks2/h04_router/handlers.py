"""Request handlers. Each returns a (status, body) tuple."""

USERS = {7: "Alice", 12: "Bob"}
ORDERS = {12: "3 widgets", 31: "1 gadget"}


def get_user(id):
    # int params can come through negative; guard against it here.
    if id < 0:
        return 404, "not found"
    if id not in USERS:
        return 404, "not found"
    return 200, f"user {USERS[id]}"


def get_order(id):
    return 200, f"order {ORDERS[id]}"


def health():
    return 200, "ok"
