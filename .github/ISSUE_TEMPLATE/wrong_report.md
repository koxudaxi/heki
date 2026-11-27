---
name: Wrong report
about: A report that is wrong about the code, or a report heki misses
title: "[report] "
labels: bug
---

A report that is marked exact but is wrong about the code is the most important kind of bug. Please file it here.

**Kind**
- [ ] heki reports something that is not true of the code
- [ ] heki misses something it should report

**Rule code**
For example `EFF101`.

**Exact or heuristic**
As printed in the report.

**Version**
`heki --version`:

**Minimal input**
The smallest Python code that shows it. Share only what you may publish.

```python

```

**Command and configuration**
```
$ heki check ...
```

**Why the claim is wrong, or what it misses**
Say what the code really does, and what the rule should have said.
