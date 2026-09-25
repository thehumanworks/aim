"""The gate's optional, narrowly allowed loopback port for the wire recorder."""

from __future__ import annotations


def fixed_port(raw: str | None) -> int | None:
    """Parse a gate-selected port; absence keeps the benchmark's ephemeral-port default."""
    if raw is None:
        return None
    if not raw.isascii() or not raw.isdecimal():
        raise ValueError("AIM_GATE_BENCH_PORT must be a decimal TCP port")
    port = int(raw)
    if not 1 <= port <= 65535:
        raise ValueError("AIM_GATE_BENCH_PORT must be between 1 and 65535")
    return port


def require_fixed_port(selected: int, raw: str | None) -> None:
    """Refuse a recorder that would listen outside the gate's allowed port."""
    required = fixed_port(raw)
    if required is not None and selected != required:
        raise ValueError("recorder port differs from AIM_GATE_BENCH_PORT")
