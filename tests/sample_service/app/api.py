import stripe

from app.models import Order
from app.store import add_item


async def checkout(order: Order) -> None:
    add_item(order, "receipt")
    stripe.Charge.create(amount=len(order.items))
