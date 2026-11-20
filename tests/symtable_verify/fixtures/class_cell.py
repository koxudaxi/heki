"""Explicit reads of the implicit `__class__` cell."""


def module_function():
    return __class__


class Owner:
    def method(self):
        return __class__

    def nested(self):
        def inner():
            return __class__

        return inner

    def uses_super(self):
        return super().__init__()

    attr = (lambda: __class__)

    class Inner:
        def method(self):
            return __class__

    class InnerBody:
        outer_cell = __class__


def explicit_class_parameter(__class__):
    return __class__
