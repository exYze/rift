"""Wire the routes together and expose dispatch(path)."""
from router import Router
from middleware import wrap
import handlers

router = Router()
router.add("/users/<int:id>", handlers.get_user)
router.add("/orders/<int:id>", handlers.get_order)
router.add("/health", handlers.health)


def dispatch(path):
    """Route a request path to its handler; 404 when nothing matches."""
    handler, params = router.match(path)
    if handler is None:
        return 404, "not found"
    return wrap(handler)(**params)


if __name__ == "__main__":
    for p in ("/users/7", "/orders/12", "/health"):
        print(p, "->", dispatch(p))
