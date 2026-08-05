#!/usr/bin/env python3

import argparse
import importlib.util
import pathlib
import re
import sys


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Verify that the Gecko adapter exports the patched CEF API hash."
    )
    parser.add_argument("cef_root", type=pathlib.Path)
    parser.add_argument("adapter_source", type=pathlib.Path)
    args = parser.parse_args()

    tools = args.cef_root / "tools"
    sys.path.insert(0, str(tools))
    module_path = tools / "cef_api_hash.py"
    spec = importlib.util.spec_from_file_location("deb_cef_api_hash", module_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)

    hashes = module.CefApiHasher(str(args.cef_root / "include"), None).calculate(
        module.EXP_VERSION, []
    )
    generated = hashes["linux"]
    source = args.adapter_source.read_text(encoding="utf-8")
    match = re.search(
        r'API_HASH_EXPERIMENTAL_LINUX: &\[u8\] = b"([0-9a-f]{40})\\0";',
        source,
    )
    if match is None:
        raise RuntimeError(
            f"cannot find API_HASH_EXPERIMENTAL_LINUX in {args.adapter_source}"
        )
    adapter = match.group(1)
    if adapter != generated:
        raise RuntimeError(
            f"Gecko adapter API hash {adapter} does not match patched CEF {generated}"
        )
    print(f"CEF experimental Linux API hash verified: {generated}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
