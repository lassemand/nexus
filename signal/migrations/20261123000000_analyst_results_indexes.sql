-- Indexes for the "Analyst results" Grafana dashboard (NEX-269): its panels filter
-- closed trades by exit time and decisions by decision time.
CREATE INDEX simulated_trades_closed_exit_idx
    ON simulated_trades (exit_at) WHERE status = 'closed';
CREATE INDEX analyst_decisions_decided_idx
    ON analyst_decisions (decided_at DESC);
