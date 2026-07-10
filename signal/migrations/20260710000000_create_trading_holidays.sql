-- Trading holiday calendar.
--
-- Stores exchange-specific trading exceptions (full closures and early-close
-- sessions). Weekends are not stored — callers handle those separately.
--
-- Update procedure: when an exchange publishes its next year's trading
-- calendar (Nasdaq Nordic publishes annually as a PDF), INSERT the new rows.
-- No code change or redeploy required.
--
-- country      : ISO 3166-1 alpha-2 country code (e.g. SE, US)
-- date         : the calendar date
-- status       : 'closed' | 'half_day'
--                'closed'   = full closure (public holiday)
--                'half_day' = early close session (e.g. 13:00 CET on Nasdaq Stockholm)

CREATE TABLE trading_holidays (
    country     TEXT        NOT NULL,
    date         DATE        NOT NULL,
    status       TEXT        NOT NULL CHECK (status IN ('closed', 'half_day')),
    note         TEXT,
    PRIMARY KEY (country, date)
);

-- ── Sweden (SE) 2015–2027 ──
-- Source: Nasdaq Nordic annual trading calendar PDFs.
-- Weekday holidays only; weekend dates omitted.

INSERT INTO trading_holidays (country, date, status, note) VALUES

-- 2015
('SE', '2015-01-01', 'closed',   'New Year''s Day'),
('SE', '2015-04-03', 'closed',   'Good Friday'),
('SE', '2015-04-06', 'closed',   'Easter Monday'),
('SE', '2015-05-01', 'closed',   'Labour Day'),
('SE', '2015-05-14', 'closed',   'Ascension Thursday'),
('SE', '2015-06-19', 'closed',   'Midsummer Eve'),
('SE', '2015-12-24', 'half_day', 'Christmas Eve'),
('SE', '2015-12-25', 'closed',   'Christmas Day'),
('SE', '2015-12-31', 'half_day', 'New Year''s Eve'),
-- Dec 26 = Sat, omitted

-- 2016
('SE', '2016-01-01', 'closed',   'New Year''s Day'),
('SE', '2016-03-25', 'closed',   'Good Friday'),
('SE', '2016-03-28', 'closed',   'Easter Monday'),
-- May 1 = Sun, omitted
('SE', '2016-05-05', 'closed',   'Ascension Thursday'),
('SE', '2016-06-06', 'closed',   'National Day'),
('SE', '2016-06-24', 'closed',   'Midsummer Eve'),
('SE', '2016-12-26', 'closed',   'Boxing Day'),
-- Dec 24 = Sat, Dec 25 = Sun, Dec 31 = Sat, omitted

-- 2017
-- Jan 1 = Sun, omitted
('SE', '2017-04-14', 'closed',   'Good Friday'),
('SE', '2017-04-17', 'closed',   'Easter Monday'),
('SE', '2017-05-01', 'closed',   'Labour Day'),
('SE', '2017-05-25', 'closed',   'Ascension Thursday'),
('SE', '2017-06-06', 'closed',   'National Day'),
('SE', '2017-06-23', 'closed',   'Midsummer Eve'),
('SE', '2017-12-25', 'closed',   'Christmas Day'),
('SE', '2017-12-26', 'closed',   'Boxing Day'),
-- Dec 24 = Sun, Dec 31 = Sun, omitted

-- 2018
('SE', '2018-01-01', 'closed',   'New Year''s Day'),
('SE', '2018-03-30', 'closed',   'Good Friday'),
('SE', '2018-04-02', 'closed',   'Easter Monday'),
('SE', '2018-05-01', 'closed',   'Labour Day'),
('SE', '2018-05-10', 'closed',   'Ascension Thursday'),
('SE', '2018-06-06', 'closed',   'National Day'),
('SE', '2018-06-22', 'closed',   'Midsummer Eve'),
('SE', '2018-12-24', 'half_day', 'Christmas Eve'),
('SE', '2018-12-25', 'closed',   'Christmas Day'),
('SE', '2018-12-26', 'closed',   'Boxing Day'),
('SE', '2018-12-31', 'half_day', 'New Year''s Eve'),

-- 2019
('SE', '2019-01-01', 'closed',   'New Year''s Day'),
('SE', '2019-04-19', 'closed',   'Good Friday'),
('SE', '2019-04-22', 'closed',   'Easter Monday'),
('SE', '2019-05-01', 'closed',   'Labour Day'),
('SE', '2019-05-30', 'closed',   'Ascension Thursday'),
('SE', '2019-06-06', 'closed',   'National Day'),
('SE', '2019-06-21', 'closed',   'Midsummer Eve'),
('SE', '2019-12-24', 'half_day', 'Christmas Eve'),
('SE', '2019-12-25', 'closed',   'Christmas Day'),
('SE', '2019-12-26', 'closed',   'Boxing Day'),
('SE', '2019-12-31', 'half_day', 'New Year''s Eve'),

-- 2020
('SE', '2020-01-01', 'closed',   'New Year''s Day'),
('SE', '2020-04-10', 'closed',   'Good Friday'),
('SE', '2020-04-13', 'closed',   'Easter Monday'),
('SE', '2020-05-01', 'closed',   'Labour Day'),
('SE', '2020-05-21', 'closed',   'Ascension Thursday'),
-- Jun 6 = Sat, omitted
('SE', '2020-06-19', 'closed',   'Midsummer Eve'),
('SE', '2020-12-24', 'half_day', 'Christmas Eve'),
('SE', '2020-12-25', 'closed',   'Christmas Day'),
('SE', '2020-12-31', 'half_day', 'New Year''s Eve'),
-- Dec 26 = Sat, omitted

-- 2021
('SE', '2021-01-01', 'closed',   'New Year''s Day'),
('SE', '2021-04-02', 'closed',   'Good Friday'),
('SE', '2021-04-05', 'closed',   'Easter Monday'),
-- May 1 = Sat, Jun 6 = Sun, omitted
('SE', '2021-05-13', 'closed',   'Ascension Thursday'),
('SE', '2021-06-25', 'closed',   'Midsummer Eve'),
('SE', '2021-12-24', 'half_day', 'Christmas Eve'),
('SE', '2021-12-31', 'half_day', 'New Year''s Eve'),
-- Dec 25 = Sat, Dec 26 = Sun, omitted

-- 2022
-- Jan 1 = Sat, May 1 = Sun, omitted
('SE', '2022-04-15', 'closed',   'Good Friday'),
('SE', '2022-04-18', 'closed',   'Easter Monday'),
('SE', '2022-05-26', 'closed',   'Ascension Thursday'),
('SE', '2022-06-06', 'closed',   'National Day'),
('SE', '2022-06-24', 'closed',   'Midsummer Eve'),
('SE', '2022-12-26', 'closed',   'Boxing Day'),
-- Dec 24 = Sat, Dec 25 = Sun, Dec 31 = Sat, omitted

-- 2023
-- Jan 1 = Sun, omitted
('SE', '2023-04-07', 'closed',   'Good Friday'),
('SE', '2023-04-10', 'closed',   'Easter Monday'),
('SE', '2023-05-01', 'closed',   'Labour Day'),
('SE', '2023-05-18', 'closed',   'Ascension Thursday'),
('SE', '2023-06-06', 'closed',   'National Day'),
('SE', '2023-06-23', 'closed',   'Midsummer Eve'),
('SE', '2023-12-25', 'closed',   'Christmas Day'),
('SE', '2023-12-26', 'closed',   'Boxing Day'),
-- Dec 24 = Sun, Dec 31 = Sun, omitted

-- 2024
('SE', '2024-01-01', 'closed',   'New Year''s Day'),
('SE', '2024-03-29', 'closed',   'Good Friday'),
('SE', '2024-04-01', 'closed',   'Easter Monday'),
('SE', '2024-05-01', 'closed',   'Labour Day'),
('SE', '2024-05-09', 'closed',   'Ascension Thursday'),
('SE', '2024-06-06', 'closed',   'National Day'),
('SE', '2024-06-21', 'closed',   'Midsummer Eve'),
('SE', '2024-12-24', 'half_day', 'Christmas Eve'),
('SE', '2024-12-25', 'closed',   'Christmas Day'),
('SE', '2024-12-26', 'closed',   'Boxing Day'),
('SE', '2024-12-31', 'half_day', 'New Year''s Eve'),

-- 2025
('SE', '2025-01-01', 'closed',   'New Year''s Day'),
('SE', '2025-04-18', 'closed',   'Good Friday'),
('SE', '2025-04-21', 'closed',   'Easter Monday'),
('SE', '2025-05-01', 'closed',   'Labour Day'),
('SE', '2025-05-29', 'closed',   'Ascension Thursday'),
('SE', '2025-06-06', 'closed',   'National Day'),
('SE', '2025-06-20', 'closed',   'Midsummer Eve'),
('SE', '2025-12-24', 'half_day', 'Christmas Eve'),
('SE', '2025-12-25', 'closed',   'Christmas Day'),
('SE', '2025-12-26', 'closed',   'Boxing Day'),
('SE', '2025-12-31', 'half_day', 'New Year''s Eve'),

-- 2026
('SE', '2026-01-01', 'closed',   'New Year''s Day'),
('SE', '2026-04-03', 'closed',   'Good Friday'),
('SE', '2026-04-06', 'closed',   'Easter Monday'),
('SE', '2026-05-01', 'closed',   'Labour Day'),
('SE', '2026-05-14', 'closed',   'Ascension Thursday'),
-- Jun 6 = Sat, omitted
('SE', '2026-06-19', 'closed',   'Midsummer Eve'),
('SE', '2026-12-24', 'half_day', 'Christmas Eve'),
('SE', '2026-12-25', 'closed',   'Christmas Day'),
('SE', '2026-12-31', 'half_day', 'New Year''s Eve'),
-- Dec 26 = Sat, omitted

-- 2027
('SE', '2027-01-01', 'closed',   'New Year''s Day'),
('SE', '2027-03-26', 'closed',   'Good Friday'),
('SE', '2027-03-29', 'closed',   'Easter Monday'),
-- May 1 = Sat, Jun 6 = Sun, omitted
('SE', '2027-05-06', 'closed',   'Ascension Thursday'),
('SE', '2027-06-25', 'closed',   'Midsummer Eve'),
('SE', '2027-12-24', 'half_day', 'Christmas Eve'),
('SE', '2027-12-31', 'half_day', 'New Year''s Eve');
-- Dec 25 = Sat, Dec 26 = Sun, omitted

-- ── United States (US) 2015–2027 ────────────────────────────────────
-- Source: Nager.Date API (date.nager.at), filtered to NYSE-observed holidays.
-- NYSE observes: New Year's Day, MLK Day, Presidents Day, Good Friday,
-- Memorial Day, Juneteenth (from 2021), Independence Day, Labor Day,
-- Thanksgiving, Christmas Day.
-- NOT observed: Lincoln's Birthday, Truman Day, Columbus Day, Veterans Day.
-- Observed dates account for Saturday→Friday and Sunday→Monday substitution.
-- XNAS (Nasdaq US) uses the same calendar as XNYS.

INSERT INTO trading_holidays (country, date, status, note) VALUES
('US', '2015-01-01', 'closed', 'New Year''s Day'),
('US', '2015-01-19', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2015-02-16', 'closed', 'Presidents Day'),
('US', '2015-04-03', 'closed', 'Good Friday'),
('US', '2015-05-25', 'closed', 'Memorial Day'),
('US', '2015-07-03', 'closed', 'Independence Day'),
('US', '2015-09-07', 'closed', 'Labour Day'),
('US', '2015-11-26', 'closed', 'Thanksgiving Day'),
('US', '2015-12-25', 'closed', 'Christmas Day'),
('US', '2016-01-01', 'closed', 'New Year''s Day'),
('US', '2016-01-18', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2016-02-15', 'closed', 'Presidents Day'),
('US', '2016-03-25', 'closed', 'Good Friday'),
('US', '2016-05-30', 'closed', 'Memorial Day'),
('US', '2016-07-04', 'closed', 'Independence Day'),
('US', '2016-09-05', 'closed', 'Labour Day'),
('US', '2016-11-24', 'closed', 'Thanksgiving Day'),
('US', '2016-12-26', 'closed', 'Christmas Day'),
('US', '2017-01-02', 'closed', 'New Year''s Day'),
('US', '2017-01-16', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2017-02-20', 'closed', 'Presidents Day'),
('US', '2017-04-14', 'closed', 'Good Friday'),
('US', '2017-05-29', 'closed', 'Memorial Day'),
('US', '2017-07-04', 'closed', 'Independence Day'),
('US', '2017-09-04', 'closed', 'Labour Day'),
('US', '2017-11-23', 'closed', 'Thanksgiving Day'),
('US', '2017-12-25', 'closed', 'Christmas Day'),
('US', '2018-01-01', 'closed', 'New Year''s Day'),
('US', '2018-01-15', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2018-02-19', 'closed', 'Presidents Day'),
('US', '2018-03-30', 'closed', 'Good Friday'),
('US', '2018-05-28', 'closed', 'Memorial Day'),
('US', '2018-07-04', 'closed', 'Independence Day'),
('US', '2018-09-03', 'closed', 'Labour Day'),
('US', '2018-11-22', 'closed', 'Thanksgiving Day'),
('US', '2018-12-25', 'closed', 'Christmas Day'),
('US', '2019-01-01', 'closed', 'New Year''s Day'),
('US', '2019-01-21', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2019-02-18', 'closed', 'Presidents Day'),
('US', '2019-04-19', 'closed', 'Good Friday'),
('US', '2019-05-27', 'closed', 'Memorial Day'),
('US', '2019-07-04', 'closed', 'Independence Day'),
('US', '2019-09-02', 'closed', 'Labour Day'),
('US', '2019-11-28', 'closed', 'Thanksgiving Day'),
('US', '2019-12-25', 'closed', 'Christmas Day'),
('US', '2020-01-01', 'closed', 'New Year''s Day'),
('US', '2020-01-20', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2020-02-17', 'closed', 'Presidents Day'),
('US', '2020-04-10', 'closed', 'Good Friday'),
('US', '2020-05-25', 'closed', 'Memorial Day'),
('US', '2020-07-03', 'closed', 'Independence Day'),
('US', '2020-09-07', 'closed', 'Labour Day'),
('US', '2020-11-26', 'closed', 'Thanksgiving Day'),
('US', '2020-12-25', 'closed', 'Christmas Day'),
('US', '2021-01-01', 'closed', 'New Year''s Day'),
('US', '2021-01-18', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2021-02-15', 'closed', 'Presidents Day'),
('US', '2021-04-02', 'closed', 'Good Friday'),
('US', '2021-05-31', 'closed', 'Memorial Day'),
('US', '2021-06-18', 'closed', 'Juneteenth National Independence Day'),
('US', '2021-07-05', 'closed', 'Independence Day'),
('US', '2021-09-06', 'closed', 'Labour Day'),
('US', '2021-11-25', 'closed', 'Thanksgiving Day'),
('US', '2021-12-24', 'closed', 'Christmas Day'),
('US', '2021-12-31', 'closed', 'New Year''s Day'),
('US', '2022-01-17', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2022-02-21', 'closed', 'Presidents Day'),
('US', '2022-04-15', 'closed', 'Good Friday'),
('US', '2022-05-30', 'closed', 'Memorial Day'),
('US', '2022-06-20', 'closed', 'Juneteenth National Independence Day'),
('US', '2022-07-04', 'closed', 'Independence Day'),
('US', '2022-09-05', 'closed', 'Labour Day'),
('US', '2022-11-24', 'closed', 'Thanksgiving Day'),
('US', '2022-12-26', 'closed', 'Christmas Day'),
('US', '2023-01-02', 'closed', 'New Year''s Day'),
('US', '2023-01-16', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2023-02-20', 'closed', 'Presidents Day'),
('US', '2023-04-07', 'closed', 'Good Friday'),
('US', '2023-05-29', 'closed', 'Memorial Day'),
('US', '2023-06-19', 'closed', 'Juneteenth National Independence Day'),
('US', '2023-07-04', 'closed', 'Independence Day'),
('US', '2023-09-04', 'closed', 'Labour Day'),
('US', '2023-11-23', 'closed', 'Thanksgiving Day'),
('US', '2023-12-25', 'closed', 'Christmas Day'),
('US', '2024-01-01', 'closed', 'New Year''s Day'),
('US', '2024-01-15', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2024-02-19', 'closed', 'Presidents Day'),
('US', '2024-03-29', 'closed', 'Good Friday'),
('US', '2024-05-27', 'closed', 'Memorial Day'),
('US', '2024-06-19', 'closed', 'Juneteenth National Independence Day'),
('US', '2024-07-04', 'closed', 'Independence Day'),
('US', '2024-09-02', 'closed', 'Labour Day'),
('US', '2024-11-28', 'closed', 'Thanksgiving Day'),
('US', '2024-12-25', 'closed', 'Christmas Day'),
('US', '2025-01-01', 'closed', 'New Year''s Day'),
('US', '2025-01-20', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2025-02-17', 'closed', 'Presidents Day'),
('US', '2025-04-18', 'closed', 'Good Friday'),
('US', '2025-05-26', 'closed', 'Memorial Day'),
('US', '2025-06-19', 'closed', 'Juneteenth National Independence Day'),
('US', '2025-07-04', 'closed', 'Independence Day'),
('US', '2025-09-01', 'closed', 'Labour Day'),
('US', '2025-11-27', 'closed', 'Thanksgiving Day'),
('US', '2025-12-25', 'closed', 'Christmas Day'),
('US', '2026-01-01', 'closed', 'New Year''s Day'),
('US', '2026-01-19', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2026-02-16', 'closed', 'Presidents Day'),
('US', '2026-04-03', 'closed', 'Good Friday'),
('US', '2026-05-25', 'closed', 'Memorial Day'),
('US', '2026-06-19', 'closed', 'Juneteenth National Independence Day'),
('US', '2026-07-03', 'closed', 'Independence Day'),
('US', '2026-09-07', 'closed', 'Labour Day'),
('US', '2026-11-26', 'closed', 'Thanksgiving Day'),
('US', '2026-12-25', 'closed', 'Christmas Day'),
('US', '2027-01-01', 'closed', 'New Year''s Day'),
('US', '2027-01-18', 'closed', 'Martin Luther King, Jr. Day'),
('US', '2027-02-15', 'closed', 'Presidents Day'),
('US', '2027-03-26', 'closed', 'Good Friday'),
('US', '2027-05-31', 'closed', 'Memorial Day'),
('US', '2027-06-18', 'closed', 'Juneteenth National Independence Day'),
('US', '2027-07-05', 'closed', 'Independence Day'),
('US', '2027-09-06', 'closed', 'Labour Day'),
('US', '2027-11-25', 'closed', 'Thanksgiving Day'),
('US', '2027-12-24', 'closed', 'Christmas Day');
