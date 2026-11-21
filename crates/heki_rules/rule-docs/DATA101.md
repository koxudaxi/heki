## What it does
Checks loops that grow a DataFrame or array by re-concatenating it on every iteration:
`df = pd.concat([df, chunk])`, `arr = np.vstack([arr, row])`, `arr = np.append(arr, x)`, and
the other quadratic reallocations of the ecosystem knowledge base. Only loop-carried
accumulation is reported: the result feeds the next iteration and is computed from the previous
value, also when it grows on some iterations only. An update through a project function counts
when that function returns a reallocation of the argument carrying the previous value, directly
or through further helpers (`acc = grow(acc)` where `grow` returns `np.append(acc, x)`), also
when an `isinstance` test picks the method (`if isinstance(g, Grower): acc = g.grow(acc)`) or an
offloading API runs it (`acc = await asyncio.to_thread(grow, acc)`); a helper that reallocates
something else and returns its argument unchanged does not. One-off
concatenations inside a loop are not reported, also on a branch that leaves the variable
unchanged otherwise (`if step: x = np.append(edges, last)`), nor is writing into a buffer passed
as `out=` (`out = np.concatenate(([a], [b]), out=out)` reuses the same buffer): the previous
value must be among the data that is combined. A list holding it counts (`pd.concat([df] +
parts)`), arithmetic on it does not (`x = np.concatenate((x - np.outer(u, v), [a]))`,
`q = np.concatenate([[w * q[0]], w * q[1:]])`): every entry of the new value changed, so each
iteration already works in proportion to the value's size, and there are no unchanged parts to
collect and combine once. A loop whose trip count the code fixes is not reported either
(`for is_p_wave in [False, True]`, `for _ in range(steps)` with `steps = 8`): it copies a fixed
number of times, whatever the input.

## What a report states

A report states that the value is rebuilt from its previous value by the reallocating call on every
iteration of the loop that completes, so each such iteration copies all of it. Its exact reports
(those the rule shows by default once it is certified) also name what the loop steps through: a
parameter, a part of one, the rows of a query, the lines of a file, or the entries of a directory.

Its premises:

- **Loop-carried**: every path around the back edge carries the new value, computed from the
  previous one (no path resets the variable). Open in a heuristic report: one path that updates
  it is enough.
- **On every iteration**: the reallocating call returns on every iteration that completes,
  exception edges into handlers included. Open in a heuristic report: one that runs on some
  iterations only counts.
- **Origin**: the loop steps through items the data decides (as for `EFF103`), and the code does
  not fix its trip count. Open in a heuristic report: a trip count heki cannot bound counts as
  data-sized.
- **Reallocates**: the call copies all of its inputs into a new value on every call, as the
  ecosystem catalog describes it. Open in a heuristic report: a project function that
  reallocates the value it is handed may return another value on some path.

Not part of the claim: how large the value grows. An exact report gives no advice: collecting
the parts first does not fit a loop that reads the value on every iteration.

## Why is this bad?
Every concatenation copies everything accumulated so far, so the loop is quadratic in the total
size.

## Example
```python
import pandas as pd


def load(chunks: list[pd.DataFrame]) -> pd.DataFrame:
    df = pd.DataFrame()
    for chunk in chunks:
        df = pd.concat([df, chunk])  # DATA101
    return df
```

Use instead:
```python
def load(chunks: list[pd.DataFrame]) -> pd.DataFrame:
    return pd.concat(chunks) if chunks else pd.DataFrame()  # `pd.concat([])` raises
```

## Fix safety
No automatic fix.
