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


def counted_up(get):
    i = 1
    while i < 6:
        try:
            return get()
        except OSError:
            pass
        i += 1


def counted_inclusive():
    i = 0
    while i <= 10:
        i += 2


def counted_down():
    tries = 3
    while tries:
        tries -= 1


def counted_mirrored():
    n = 10
    while 0 < n:
        n -= 3


def counted_down_inclusive():
    n = 3
    while n >= 0:
        n -= 1


def counted_once():
    i = 0
    while i == 0:
        i = 1 + i


def counted_down_strict():
    n = 5
    while n > 2:
        n -= 1


def counter_tested_by_identity():
    i = 0
    while i is not 3:
        i += 1


def counter_doubled():
    i = 1
    while i < 9:
        i = i * 2


def counter_steps_differ(c):
    i = 0
    while i < 9:
        if c():
            i += 1
            continue
        i += 2


def counter_merged_before(c):
    if c:
        i = 1
    else:
        i = 2
    while i < 3:
        c()


def counted_past_small():
    i = 0
    while i < 100:
        i += 1


def counter_skipped(skip):
    i = 0
    while i < 3:
        if skip():
            continue
        i += 1


def counter_from_parameter(start):
    while start < 3:
        start += 1


def countdown_past_zero():
    n = 5
    while n:
        n -= 2


def paced_until(done, time):
    while not done():
        time.sleep(5)


def paced_attempts(get, sleep):
    for _ in range(30):
        try:
            if get():
                break
        except OSError:
            pass
        sleep(1)


def paced_forever(get, time):
    while True:
        if get():
            break
        time.sleep(15)


def paced_by_timed_wait(stop, get):
    while True:
        get()
        stop.wait(0.5)


def paced_by_timeout(stop, get, delay):
    while True:
        get()
        stop.wait(timeout=delay)


def paced_by_computed_delay(get, time, delay):
    while not get():
        time.sleep(delay)


def paced_over_items(items, get, time):
    for item in items:
        get(item)
        time.sleep(1)


def sleeps_on_one_path(get, time):
    while True:
        if get():
            continue
        time.sleep(1)


def sleeps_after_a_raise(get, time):
    while True:
        try:
            get()
            time.sleep(1)
        except OSError:
            pass


def zero_sleep(get, asyncio):
    while True:
        get()
        asyncio.sleep(0)


def waits_for_tasks(get, asyncio, tasks):
    while True:
        get()
        asyncio.wait(tasks)


def unpacked_targets(get, rows, holder):
    for first, *rest in rows:
        get(rest)
        sleep(1)
    for holder.current in rows:
        get()
        sleep(1)


def attempt_in_a_tuple(get, proxies):
    for attempt, proxy in enumerate(proxies):
        try:
            return get(proxy)
        except OSError:
            pass


class Worker:
    def serve(self):
        while self._isRunning:
            pass

    def until_shutdown(self):
        while not self.shutdown_requested:
            pass

    def until_stop_event(self):
        while not self._stop_event.is_set():
            pass

    def until_ready_event(self):
        while not self.ready.is_set():
            pass
