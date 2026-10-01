-- Self-sampling, as the README's Monitoring section suggests for pooled
-- connections: this backend's pg_automerge counters and the totals of its
-- Postgres memory contexts, between two statements of the workload.
INSERT INTO soak_mem (pid, allocated_bytes, peak_allocated_bytes, live_documents, loads, load_time, contexts_total, contexts_used) SELECT pg_backend_pid(), m.allocated_bytes, m.peak_allocated_bytes, m.live_documents, m.loads, m.load_time, c.total, c.used FROM automerge_memory_usage() m, (SELECT sum(total_bytes) AS total, sum(used_bytes) AS used FROM pg_backend_memory_contexts) c;
