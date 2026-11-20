"""Loop trip counts and caps the code fixes (`heki_ir::facts`), pinned by e2e_corpus_regressions.rs."""

LIMIT = 8
WIDTH = LIMIT * 2 - 15
STEP = LIMIT // 2
gone = ["x"]
del gone


def strings():
    for c in "abc":
        pass


def sorted_keys():
    for k in sorted({"a": 1, "b": 2}, key=str):
        pass


def reversed_list():
    for x in reversed([1, 2, 3]):
        pass


def mapped(f):
    for x in map(f, (1, 2)):
        pass


def filtered(f):
    for x in filter(f, [1, 2, 3]):
        pass


def other_builtin(xs):
    for x in min(xs, [1]):
        pass


def enumerate_start():
    for i, x in enumerate([1, 2], start=1):
        pass


def zip_strict(xs):
    for a, b in zip(xs, (1, 2), strict=True):
        pass


def negative():
    for i in range(-WIDTH, LIMIT - 6):
        pass


def added():
    for i in range(LIMIT + 0):
        pass


def unary_plus():
    for i in range(+LIMIT - 5):
        pass


def floor_division():
    for i in range(STEP):
        pass


def local_constant():
    n = 3
    for i in range(n):
        pass


def deleted():
    for x in gone:
        pass


def deep_copies():
    r0 = range(3)
    r1 = r0
    r2 = r1
    r3 = r2
    r4 = r3
    r5 = r4
    r6 = r5
    r7 = r6
    r8 = r7
    r9 = r8
    r10 = r9
    r11 = r10
    r12 = r11
    r13 = r12
    r14 = r13
    r15 = r14
    r16 = r15
    r17 = r16
    for i in r17:
        pass


def deep_ints():
    n0 = 3
    n1 = n0
    n2 = n1
    n3 = n2
    n4 = n3
    n5 = n4
    n6 = n5
    n7 = n6
    n8 = n7
    n9 = n8
    n10 = n9
    n11 = n10
    n12 = n11
    n13 = n12
    n14 = n13
    n15 = n14
    n16 = n15
    n17 = n16
    for i in range(n17):
        pass


def deep_display():
    a0 = [1]
    a1 = a0
    a2 = a1
    a3 = a2
    a4 = a3
    a5 = a4
    a6 = a5
    a7 = a6
    a8 = a7
    a9 = a8
    a10 = a9
    a11 = a10
    a12 = a11
    a13 = a12
    a14 = a13
    a15 = a14
    a16 = a15
    a17 = a16
    a18 = a17
    a19 = a18
    a20 = a19
    a21 = a20
    a22 = a21
    a23 = a22
    a24 = a23
    a25 = a24
    a26 = a25
    a27 = a26
    a28 = a27
    a29 = a28
    a30 = a29
    a31 = a30
    a32 = a31
    a33 = a32
    for x in a33:
        pass


def cap_gt(read):
    buf = b""
    while 10 > len(buf):
        buf += read()


def cap_lte(read):
    buf = b""
    while len(buf) <= 9:
        buf += read()


def cap_ne(read):
    buf = b""
    while len(buf) != 10:
        buf += read()


def cap_positive(read):
    buf = b"x"
    while len(buf) > 0:
        buf = buf[1:]


def cap_starred(read, parts):
    buf = b""
    while len(*parts) < 10:
        buf += read()


def shadowed_range(range):
    for _ in range(1):
        pass


def grown_constructor(items):
    xs = list("a")
    xs.extend(items)
    for x in xs:
        pass


def ungrown_constructor():
    xs = list("ab")
    for x in xs:
        pass


def returned_alias(items):
    xs = []
    other = min(xs, xs)
    other.extend(items)
    for x in xs:
        pass


def generator_over_range():
    return sum(1 for _ in range(3))


def drained_queue(inactive_users):
    while inactive_users:
        inactive_users.pop()


def drained_active(active_users):
    while active_users:
        active_users.pop()


class Worker:
    def serve(self):
        while self._isRunning:
            pass

    def until_shutdown(self):
        while not self.shutdown_requested:
            pass
