COMMENT ON COLUMN special_events.source IS
    'Where the row comes from: ''mfn'' (an MFN item, or a row from the MFN company page calendar) '
    'or ''mfn_release'' (a scheduled row from a company''s own financial calendar release); NULL for legacy events.';
COMMENT ON COLUMN special_events.superseded_by IS
    'On occurred rows: the latest correction of the report (within 7 days). '
    'On scheduled rows: the row of the other calendar source (page or release) that wins for the same report, '
    'so readers use scheduled rows with superseded_by IS NULL.';
