class Order:
    def __init__(self, order_id: int) -> None:
        self.order_id = order_id
        self.items: list[str] = []
