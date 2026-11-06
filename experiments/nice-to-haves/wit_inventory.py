#!/usr/bin/env python3
"""Validate and inventory an existing component with pinned wasm-tools 1.263.0."""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile

from benchmark_import import digest, pinned_tool
from toolkit import inspect_wasm

WASM_TOOLS_VERSION = "wasm-tools 1.263.0"


def inventory(component, tool):
    tool = Path(tool).resolve(strict=True)
    identity = pinned_tool(tool, WASM_TOOLS_VERSION)
    data = Path(component).read_bytes()
    envelope = inspect_wasm(data)
    if envelope["kind"] != "component":
        raise ValueError("WIT inventory requires a component, not a core module")
    # Validate and decode exactly the bytes hashed, even if a build replaces the original path.
    with tempfile.TemporaryDirectory(prefix="nice-wit-") as directory:
        snapshot = Path(directory) / "input.wasm"
        snapshot.write_bytes(data)
        subprocess.run([str(tool), "validate", "--features", "all", str(snapshot)],
                       check=True, capture_output=True, timeout=30)
        result = subprocess.run([str(tool), "component", "wit", "--json", str(snapshot)],
                                check=True, capture_output=True, timeout=30)
    graph = json.loads(result.stdout)

    def describe(key, item):
        if "interface" in item:
            interface = graph["interfaces"][item["interface"]["id"]]
            package_id = interface.get("package")
            package = graph["packages"][package_id]["name"] if package_id is not None else None
            return {"world_key": key, "kind": "interface", "name": interface["name"], "package": package,
                    "functions": sorted(interface["functions"]), "types": sorted(interface["types"])}
        if "function" in item:
            return {"world_key": key, "kind": "function", "name": item["function"]["name"]}
        if "type" in item:
            return {"world_key": key, "kind": "type", "type_id": item["type"]}
        raise ValueError("unknown WIT world item in pinned schema")

    worlds = []
    for world in graph["worlds"]:
        worlds.append({"name": world["name"], **{
            direction: [describe(key, item) for key, item in sorted(world[direction].items())]
            for direction in ("imports", "exports")
        }})
    return {
        "schema": "nanovm-nice-wit-v1", "tool": identity,
        "component": {"sha256": digest(data), "bytes": len(data)},
        "validation_features": "all", "worlds": worlds,
        "packages_in_type_graph": sorted(p["name"] for p in graph["packages"]),
        "scope": "external component interface only; type-graph packages are not a runtime dependency or SBOM list",
        "limitations": ["Validation with all proposals does not establish nanovm compatibility or safe execution.",
                        "Composed internal providers may disappear from external imports; nested code is not inventoried."]
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("component", type=Path)
    parser.add_argument("--wasm-tools", type=Path, required=True)
    args = parser.parse_args()
    try:
        print(json.dumps(inventory(args.component, args.wasm_tools), indent=2, sort_keys=True))
    except (OSError, ValueError, KeyError, IndexError, TypeError, subprocess.SubprocessError) as error:
        parser.exit(2, f"component/tool failure: {type(error).__name__}\n")


if __name__ == "__main__":
    main()
