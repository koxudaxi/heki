import importlib
import sys
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import collections
try:
    import module_that_is_not_installed
except ImportError:
    module_that_is_not_installed = None
if sys.platform == "a-platform-that-does-not-exist":
    import winreg
for plugin in ["alpha"]:
    importlib.import_module("basic.plugins." + plugin)


class Holder:
    import decimal

    fractions = __import__("fractions")


def later():
    import textwrap

    return textwrap


PARSER = importlib.import_module(".parse", __package__)
STRING = importlib.import_module("string")
from basic import parse as parse_function
from basic import opened
from os.path import *
