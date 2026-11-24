import sys

lazy import fractions as dep
lazy import decimal as dep
NUMBER = dep.Decimal

lazy import zipfile
try:
    zipfile.ZipFile
except ImportError:
    pass

if sys.version_info >= (3, 15):
    lazy import tomllib as toml
else:
    lazy import plistlib as toml
LOADS = toml.loads
