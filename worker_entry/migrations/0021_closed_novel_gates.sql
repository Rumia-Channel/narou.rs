-- Metadata snapshots read only closed transfer gates. Open works are absent
-- from this covering index, including during steady-state library requests.
CREATE INDEX IF NOT EXISTS novel_access_gates_closed_idx
    ON novel_access_gates(closed) WHERE closed=1;
