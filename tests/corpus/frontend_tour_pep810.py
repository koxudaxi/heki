"""heki frontend syntax tour: Python 3.15 syntax (PEP 810 lazy imports, PEP 798 unpacking)."""

lazy import json
lazy import xml.etree.ElementTree as ET
lazy from collections import OrderedDict, defaultdict as dd
import os


def use_lazy(rows, groups):
    flat = [*row for row in rows]
    unique = {*row for row in rows}
    merged = {**group for group in groups}
    return json.dumps(flat), ET, OrderedDict(merged), dd(list), unique, os.sep
