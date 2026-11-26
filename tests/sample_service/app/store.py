from app.models import Order


def add_item(order: Order, item: str) -> None:
    order.items.append(item)


def item_count(order: Order) -> int:
    return len(order.items)
