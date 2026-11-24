import os

if os.environ.get("CYC_BRANCH_NEVER_SET"):
    import cyc_branch.b


def helper_a() -> int:
    return 1
