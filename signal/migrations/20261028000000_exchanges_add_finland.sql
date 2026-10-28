-- Finland: Nasdaq Helsinki (XHEL) and First North Finland (FNFI).
-- Sessions are 10:00-18:30 local, the end of the closing auction; no half days.
-- Prices are in EUR.
INSERT INTO exchanges (mic, country, timezone, open_local, close_local, half_day_close_local, currency) VALUES
    ('XHEL', 'FI', 'Europe/Helsinki', '10:00', '18:30', NULL, 'EUR'),
    ('FNFI', 'FI', 'Europe/Helsinki', '10:00', '18:30', NULL, 'EUR');
