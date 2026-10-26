-- Exchange facts: country, timezone, regular session hours and currency.
-- `alpha::calendar::EXCHANGE_TO_COUNTRY` must agree with this table (tested).
CREATE TABLE exchanges (
    mic                  TEXT PRIMARY KEY,
    country              TEXT NOT NULL,
    timezone             TEXT NOT NULL,
    open_local           TIME NOT NULL,
    close_local          TIME NOT NULL,
    half_day_close_local TIME NULL,
    currency             TEXT NOT NULL
);

INSERT INTO exchanges (mic, country, timezone, open_local, close_local, half_day_close_local, currency) VALUES
    ('FNSE', 'SE', 'Europe/Stockholm', '09:00', '17:30', '13:00', 'SEK'),
    ('XSTO', 'SE', 'Europe/Stockholm', '09:00', '17:30', '13:00', 'SEK'),
    ('XCSE', 'DK', 'Europe/Copenhagen', '09:00', '17:00', NULL, 'DKK'),
    ('FNDK', 'DK', 'Europe/Copenhagen', '09:00', '17:00', NULL, 'DKK');
