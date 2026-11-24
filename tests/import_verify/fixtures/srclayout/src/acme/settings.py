import os

DEBUG = os.environ.get("DEBUG") == "1"
try:
    import tomllib
except ModuleNotFoundError:
    tomllib = None
