"""Comprehensions in annotation scopes of a class body are not inlined (they see the class)."""


class Holder:
    type Alias = list[[i for i in range(2)]]
    type Bounded[T: [j for j in range(2)]] = T

    def generic[U: [k for k in range(2)]](self, value: [m for m in range(2)]) -> U:
        return value
