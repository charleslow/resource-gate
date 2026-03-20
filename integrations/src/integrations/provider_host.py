"""
Provider host process — the bridge between the Rust core and Python providers.

Reads JSON commands from stdin, dispatches to the appropriate ComputeProvider
method, writes JSON responses to stdout. One instance per provider.
"""
