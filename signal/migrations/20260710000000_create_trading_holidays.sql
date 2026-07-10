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

-- ── NYSE / Nasdaq US (XNYS) 2015–2027 ────────────────────────────────────
-- Source: Nager.Date API (date.nager.at), filtered to NYSE-observed holidays.
-- NYSE observes: New Year's Day, MLK Day, Presidents Day, Good Friday,
-- Memorial Day, Juneteenth (from 2021), Independence Day, Labor Day,
-- Thanksgiving, Christmas Day.
-- NOT observed: Lincoln's Birthday, Truman Day, Columbus Day, Veterans Day.
-- Observed dates account for Saturday→Friday and Sunday→Monday substitution.
-- XNAS (Nasdaq US) uses the same calendar as XNYS.

INSERT INTO trading_holidays (exchange_mic, date, status, note) VALUES
('XNYS', '2015-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2015-01-19', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2015-02-16', 'closed', 'Presidents Day'),
('XNYS', '2015-04-03', 'closed', 'Good Friday'),
('XNYS', '2015-05-25', 'closed', 'Memorial Day'),
('XNYS', '2015-07-03', 'closed', 'Independence Day'),
('XNYS', '2015-09-07', 'closed', 'Labour Day'),
('XNYS', '2015-11-26', 'closed', 'Thanksgiving Day'),
('XNYS', '2015-12-25', 'closed', 'Christmas Day'),
('XNYS', '2016-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2016-01-18', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2016-02-15', 'closed', 'Presidents Day'),
('XNYS', '2016-03-25', 'closed', 'Good Friday'),
('XNYS', '2016-05-30', 'closed', 'Memorial Day'),
('XNYS', '2016-07-04', 'closed', 'Independence Day'),
('XNYS', '2016-09-05', 'closed', 'Labour Day'),
('XNYS', '2016-11-24', 'closed', 'Thanksgiving Day'),
('XNYS', '2016-12-26', 'closed', 'Christmas Day'),
('XNYS', '2017-01-02', 'closed', 'New Year''s Day'),
('XNYS', '2017-01-16', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2017-02-20', 'closed', 'Presidents Day'),
('XNYS', '2017-04-14', 'closed', 'Good Friday'),
('XNYS', '2017-05-29', 'closed', 'Memorial Day'),
('XNYS', '2017-07-04', 'closed', 'Independence Day'),
('XNYS', '2017-09-04', 'closed', 'Labour Day'),
('XNYS', '2017-11-23', 'closed', 'Thanksgiving Day'),
('XNYS', '2017-12-25', 'closed', 'Christmas Day'),
('XNYS', '2018-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2018-01-15', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2018-02-19', 'closed', 'Presidents Day'),
('XNYS', '2018-03-30', 'closed', 'Good Friday'),
('XNYS', '2018-05-28', 'closed', 'Memorial Day'),
('XNYS', '2018-07-04', 'closed', 'Independence Day'),
('XNYS', '2018-09-03', 'closed', 'Labour Day'),
('XNYS', '2018-11-22', 'closed', 'Thanksgiving Day'),
('XNYS', '2018-12-25', 'closed', 'Christmas Day'),
('XNYS', '2019-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2019-01-21', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2019-02-18', 'closed', 'Presidents Day'),
('XNYS', '2019-04-19', 'closed', 'Good Friday'),
('XNYS', '2019-05-27', 'closed', 'Memorial Day'),
('XNYS', '2019-07-04', 'closed', 'Independence Day'),
('XNYS', '2019-09-02', 'closed', 'Labour Day'),
('XNYS', '2019-11-28', 'closed', 'Thanksgiving Day'),
('XNYS', '2019-12-25', 'closed', 'Christmas Day'),
('XNYS', '2020-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2020-01-20', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2020-02-17', 'closed', 'Presidents Day'),
('XNYS', '2020-04-10', 'closed', 'Good Friday'),
('XNYS', '2020-05-25', 'closed', 'Memorial Day'),
('XNYS', '2020-07-03', 'closed', 'Independence Day'),
('XNYS', '2020-09-07', 'closed', 'Labour Day'),
('XNYS', '2020-11-26', 'closed', 'Thanksgiving Day'),
('XNYS', '2020-12-25', 'closed', 'Christmas Day'),
('XNYS', '2021-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2021-01-18', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2021-02-15', 'closed', 'Presidents Day'),
('XNYS', '2021-04-02', 'closed', 'Good Friday'),
('XNYS', '2021-05-31', 'closed', 'Memorial Day'),
('XNYS', '2021-06-18', 'closed', 'Juneteenth National Independence Day'),
('XNYS', '2021-07-05', 'closed', 'Independence Day'),
('XNYS', '2021-09-06', 'closed', 'Labour Day'),
('XNYS', '2021-11-25', 'closed', 'Thanksgiving Day'),
('XNYS', '2021-12-24', 'closed', 'Christmas Day'),
('XNYS', '2021-12-31', 'closed', 'New Year''s Day'),
('XNYS', '2022-01-17', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2022-02-21', 'closed', 'Presidents Day'),
('XNYS', '2022-04-15', 'closed', 'Good Friday'),
('XNYS', '2022-05-30', 'closed', 'Memorial Day'),
('XNYS', '2022-06-20', 'closed', 'Juneteenth National Independence Day'),
('XNYS', '2022-07-04', 'closed', 'Independence Day'),
('XNYS', '2022-09-05', 'closed', 'Labour Day'),
('XNYS', '2022-11-24', 'closed', 'Thanksgiving Day'),
('XNYS', '2022-12-26', 'closed', 'Christmas Day'),
('XNYS', '2023-01-02', 'closed', 'New Year''s Day'),
('XNYS', '2023-01-16', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2023-02-20', 'closed', 'Presidents Day'),
('XNYS', '2023-04-07', 'closed', 'Good Friday'),
('XNYS', '2023-05-29', 'closed', 'Memorial Day'),
('XNYS', '2023-06-19', 'closed', 'Juneteenth National Independence Day'),
('XNYS', '2023-07-04', 'closed', 'Independence Day'),
('XNYS', '2023-09-04', 'closed', 'Labour Day'),
('XNYS', '2023-11-23', 'closed', 'Thanksgiving Day'),
('XNYS', '2023-12-25', 'closed', 'Christmas Day'),
('XNYS', '2024-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2024-01-15', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2024-02-19', 'closed', 'Presidents Day'),
('XNYS', '2024-03-29', 'closed', 'Good Friday'),
('XNYS', '2024-05-27', 'closed', 'Memorial Day'),
('XNYS', '2024-06-19', 'closed', 'Juneteenth National Independence Day'),
('XNYS', '2024-07-04', 'closed', 'Independence Day'),
('XNYS', '2024-09-02', 'closed', 'Labour Day'),
('XNYS', '2024-11-28', 'closed', 'Thanksgiving Day'),
('XNYS', '2024-12-25', 'closed', 'Christmas Day'),
('XNYS', '2025-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2025-01-20', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2025-02-17', 'closed', 'Presidents Day'),
('XNYS', '2025-04-18', 'closed', 'Good Friday'),
('XNYS', '2025-05-26', 'closed', 'Memorial Day'),
('XNYS', '2025-06-19', 'closed', 'Juneteenth National Independence Day'),
('XNYS', '2025-07-04', 'closed', 'Independence Day'),
('XNYS', '2025-09-01', 'closed', 'Labour Day'),
('XNYS', '2025-11-27', 'closed', 'Thanksgiving Day'),
('XNYS', '2025-12-25', 'closed', 'Christmas Day'),
('XNYS', '2026-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2026-01-19', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2026-02-16', 'closed', 'Presidents Day'),
('XNYS', '2026-04-03', 'closed', 'Good Friday'),
('XNYS', '2026-05-25', 'closed', 'Memorial Day'),
('XNYS', '2026-06-19', 'closed', 'Juneteenth National Independence Day'),
('XNYS', '2026-07-03', 'closed', 'Independence Day'),
('XNYS', '2026-09-07', 'closed', 'Labour Day'),
('XNYS', '2026-11-26', 'closed', 'Thanksgiving Day'),
('XNYS', '2026-12-25', 'closed', 'Christmas Day'),
('XNYS', '2027-01-01', 'closed', 'New Year''s Day'),
('XNYS', '2027-01-18', 'closed', 'Martin Luther King, Jr. Day'),
('XNYS', '2027-02-15', 'closed', 'Presidents Day'),
('XNYS', '2027-03-26', 'closed', 'Good Friday'),
('XNYS', '2027-05-31', 'closed', 'Memorial Day'),
('XNYS', '2027-06-18', 'closed', 'Juneteenth National Independence Day'),
('XNYS', '2027-07-05', 'closed', 'Independence Day'),
('XNYS', '2027-09-06', 'closed', 'Labour Day'),
('XNYS', '2027-11-25', 'closed', 'Thanksgiving Day'),
('XNYS', '2027-12-24', 'closed', 'Christmas Day');
