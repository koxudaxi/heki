## What it does
Checks loops that grow a DataFrame or array by re-concatenating it on every iteration:
`df = pd.concat([df, chunk])`, `arr = np.vstack([arr, row])`, `arr = np.append(arr, x)`, and
the other quadratic reallocations of the ecosystem knowledge base. Only loop-carried
accumulation is reported: the result feeds the next iteration and is computed from the previous
value, also when it grows on some iterations only. One-off concatenations inside a loop are
not, also on a branch that leaves the variable unchanged otherwise
(`if step: x = np.append(edges, last)`), nor is writing into a buffer passed as `out=`
(`out = np.concatenate(([a], [b]), out=out)` reuses the same buffer): the previous value must be
among the data that is combined.

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
    return pd.concat(chunks)
```

## Fix safety
No automatic fix.
