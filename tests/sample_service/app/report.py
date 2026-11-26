from app.models import Order


def summary(order: Order) -> str:
    return ", ".join(order.items)


def total(prices: list[float]) -> float:
    return sum(prices)
