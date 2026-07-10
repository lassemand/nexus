-- Trading holiday calendar.
--
-- Stores exchange-specific trading exceptions (full closures and early-close
-- sessions). Weekends are not stored — callers handle those separately.
--
-- Update procedure: when an exchange publishes its next year's trading
-- calendar (Nasdaq Nordic publishes annually as a PDF), INSERT the new rows.
-- No code change or redeploy required.
--
-- exchange_mic : ISO 10383 Market Identifier Code
-- date         : the calendar date
-- status       : 'closed' | 'half_day'
--                'closed'   = full closure (public holiday)
--                'half_day' = early close session (e.g. 13:00 CET on Nasdaq Stockholm)

CREATE TABLE trading_holidays (
    exchange_mic TEXT        NOT NULL,
    date         DATE        NOT NULL,
    status       TEXT        NOT NULL CHECK (status IN ('closed', 'half_day')),
    note         TEXT,
    PRIMARY KEY (exchange_mic, date)
);

-- ── Nasdaq Stockholm / First North Growth Market (FNSE/XSTO) 2015–2027 ──
-- Source: Nasdaq Nordic annual trading calendar PDFs.
-- Weekday holidays only; weekend dates omitted.

INSERT INTO trading_holidays (exchange_mic, date, status, note) VALUES

-- 2015
('FNSE', '2015-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2015-04-03', 'closed',   'Good Friday'),
('FNSE', '2015-04-06', 'closed',   'Easter Monday'),
('FNSE', '2015-05-01', 'closed',   'Labour Day'),
('FNSE', '2015-05-14', 'closed',   'Ascension Thursday'),
('FNSE', '2015-06-19', 'closed',   'Midsummer Eve'),
('FNSE', '2015-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2015-12-25', 'closed',   'Christmas Day'),
('FNSE', '2015-12-31', 'half_day', 'New Year''s Eve'),
-- Dec 26 = Sat, omitted

-- 2016
('FNSE', '2016-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2016-03-25', 'closed',   'Good Friday'),
('FNSE', '2016-03-28', 'closed',   'Easter Monday'),
-- May 1 = Sun, omitted
('FNSE', '2016-05-05', 'closed',   'Ascension Thursday'),
('FNSE', '2016-06-06', 'closed',   'National Day'),
('FNSE', '2016-06-24', 'closed',   'Midsummer Eve'),
('FNSE', '2016-12-26', 'closed',   'Boxing Day'),
-- Dec 24 = Sat, Dec 25 = Sun, Dec 31 = Sat, omitted

-- 2017
-- Jan 1 = Sun, omitted
('FNSE', '2017-04-14', 'closed',   'Good Friday'),
('FNSE', '2017-04-17', 'closed',   'Easter Monday'),
('FNSE', '2017-05-01', 'closed',   'Labour Day'),
('FNSE', '2017-05-25', 'closed',   'Ascension Thursday'),
('FNSE', '2017-06-06', 'closed',   'National Day'),
('FNSE', '2017-06-23', 'closed',   'Midsummer Eve'),
('FNSE', '2017-12-25', 'closed',   'Christmas Day'),
('FNSE', '2017-12-26', 'closed',   'Boxing Day'),
-- Dec 24 = Sun, Dec 31 = Sun, omitted

-- 2018
('FNSE', '2018-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2018-03-30', 'closed',   'Good Friday'),
('FNSE', '2018-04-02', 'closed',   'Easter Monday'),
('FNSE', '2018-05-01', 'closed',   'Labour Day'),
('FNSE', '2018-05-10', 'closed',   'Ascension Thursday'),
('FNSE', '2018-06-06', 'closed',   'National Day'),
('FNSE', '2018-06-22', 'closed',   'Midsummer Eve'),
('FNSE', '2018-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2018-12-25', 'closed',   'Christmas Day'),
('FNSE', '2018-12-26', 'closed',   'Boxing Day'),
('FNSE', '2018-12-31', 'half_day', 'New Year''s Eve'),

-- 2019
('FNSE', '2019-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2019-04-19', 'closed',   'Good Friday'),
('FNSE', '2019-04-22', 'closed',   'Easter Monday'),
('FNSE', '2019-05-01', 'closed',   'Labour Day'),
('FNSE', '2019-05-30', 'closed',   'Ascension Thursday'),
('FNSE', '2019-06-06', 'closed',   'National Day'),
('FNSE', '2019-06-21', 'closed',   'Midsummer Eve'),
('FNSE', '2019-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2019-12-25', 'closed',   'Christmas Day'),
('FNSE', '2019-12-26', 'closed',   'Boxing Day'),
('FNSE', '2019-12-31', 'half_day', 'New Year''s Eve'),

-- 2020
('FNSE', '2020-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2020-04-10', 'closed',   'Good Friday'),
('FNSE', '2020-04-13', 'closed',   'Easter Monday'),
('FNSE', '2020-05-01', 'closed',   'Labour Day'),
('FNSE', '2020-05-21', 'closed',   'Ascension Thursday'),
-- Jun 6 = Sat, omitted
('FNSE', '2020-06-19', 'closed',   'Midsummer Eve'),
('FNSE', '2020-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2020-12-25', 'closed',   'Christmas Day'),
('FNSE', '2020-12-31', 'half_day', 'New Year''s Eve'),
-- Dec 26 = Sat, omitted

-- 2021
('FNSE', '2021-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2021-04-02', 'closed',   'Good Friday'),
('FNSE', '2021-04-05', 'closed',   'Easter Monday'),
-- May 1 = Sat, Jun 6 = Sun, omitted
('FNSE', '2021-05-13', 'closed',   'Ascension Thursday'),
('FNSE', '2021-06-25', 'closed',   'Midsummer Eve'),
('FNSE', '2021-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2021-12-31', 'half_day', 'New Year''s Eve'),
-- Dec 25 = Sat, Dec 26 = Sun, omitted

-- 2022
-- Jan 1 = Sat, May 1 = Sun, omitted
('FNSE', '2022-04-15', 'closed',   'Good Friday'),
('FNSE', '2022-04-18', 'closed',   'Easter Monday'),
('FNSE', '2022-05-26', 'closed',   'Ascension Thursday'),
('FNSE', '2022-06-06', 'closed',   'National Day'),
('FNSE', '2022-06-24', 'closed',   'Midsummer Eve'),
('FNSE', '2022-12-26', 'closed',   'Boxing Day'),
-- Dec 24 = Sat, Dec 25 = Sun, Dec 31 = Sat, omitted

-- 2023
-- Jan 1 = Sun, omitted
('FNSE', '2023-04-07', 'closed',   'Good Friday'),
('FNSE', '2023-04-10', 'closed',   'Easter Monday'),
('FNSE', '2023-05-01', 'closed',   'Labour Day'),
('FNSE', '2023-05-18', 'closed',   'Ascension Thursday'),
('FNSE', '2023-06-06', 'closed',   'National Day'),
('FNSE', '2023-06-23', 'closed',   'Midsummer Eve'),
('FNSE', '2023-12-25', 'closed',   'Christmas Day'),
('FNSE', '2023-12-26', 'closed',   'Boxing Day'),
-- Dec 24 = Sun, Dec 31 = Sun, omitted

-- 2024
('FNSE', '2024-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2024-03-29', 'closed',   'Good Friday'),
('FNSE', '2024-04-01', 'closed',   'Easter Monday'),
('FNSE', '2024-05-01', 'closed',   'Labour Day'),
('FNSE', '2024-05-09', 'closed',   'Ascension Thursday'),
('FNSE', '2024-06-06', 'closed',   'National Day'),
('FNSE', '2024-06-21', 'closed',   'Midsummer Eve'),
('FNSE', '2024-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2024-12-25', 'closed',   'Christmas Day'),
('FNSE', '2024-12-26', 'closed',   'Boxing Day'),
('FNSE', '2024-12-31', 'half_day', 'New Year''s Eve'),

-- 2025
('FNSE', '2025-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2025-04-18', 'closed',   'Good Friday'),
('FNSE', '2025-04-21', 'closed',   'Easter Monday'),
('FNSE', '2025-05-01', 'closed',   'Labour Day'),
('FNSE', '2025-05-29', 'closed',   'Ascension Thursday'),
('FNSE', '2025-06-06', 'closed',   'National Day'),
('FNSE', '2025-06-20', 'closed',   'Midsummer Eve'),
('FNSE', '2025-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2025-12-25', 'closed',   'Christmas Day'),
('FNSE', '2025-12-26', 'closed',   'Boxing Day'),
('FNSE', '2025-12-31', 'half_day', 'New Year''s Eve'),

-- 2026
('FNSE', '2026-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2026-04-03', 'closed',   'Good Friday'),
('FNSE', '2026-04-06', 'closed',   'Easter Monday'),
('FNSE', '2026-05-01', 'closed',   'Labour Day'),
('FNSE', '2026-05-14', 'closed',   'Ascension Thursday'),
-- Jun 6 = Sat, omitted
('FNSE', '2026-06-19', 'closed',   'Midsummer Eve'),
('FNSE', '2026-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2026-12-25', 'closed',   'Christmas Day'),
('FNSE', '2026-12-31', 'half_day', 'New Year''s Eve'),
-- Dec 26 = Sat, omitted

-- 2027
('FNSE', '2027-01-01', 'closed',   'New Year''s Day'),
('FNSE', '2027-03-26', 'closed',   'Good Friday'),
('FNSE', '2027-03-29', 'closed',   'Easter Monday'),
-- May 1 = Sat, Jun 6 = Sun, omitted
('FNSE', '2027-05-06', 'closed',   'Ascension Thursday'),
('FNSE', '2027-06-25', 'closed',   'Midsummer Eve'),
('FNSE', '2027-12-24', 'half_day', 'Christmas Eve'),
('FNSE', '2027-12-31', 'half_day', 'New Year''s Eve');
-- Dec 25 = Sat, Dec 26 = Sun, omitted
