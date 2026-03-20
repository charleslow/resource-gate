// JSON-over-stdio bridge to Python provider processes.
// Spawns a Python process per provider, sends ResourceRequest as JSON on stdin,
// reads JobHandle/JobResult as JSON from stdout.
