# Model label evolution

## Current label (NEX-54)

```python
y = (df["post_ar_1d"] > LABEL_THRESHOLD)  # default threshold: 0.08
```

## Stronger label (use once insider_filings has ≥ 6 months of history — NEX-59)

```python
# Current label: post_ar_1d > LABEL_THRESHOLD
# Stronger label (requires ticker_event_consistency from NEX-59):
#   post_ar_1d > LABEL_THRESHOLD
#   AND mean_abvol_20d > 1.5        (volume was anomalous)
#   AND had_insider_purchase_20d    (Form 4 purchase within 20 days)
# Use this label once insider_filings has >= 6 months of history
y = (
    (df["post_ar_1d"] > LABEL_THRESHOLD)
    & (df["mean_abvol_20d"] > 1.5)
    & (df["had_insider_purchase_20d"] == 1)
)
```

The `had_insider_purchase_20d` column would be joined from `insider_filings` at training time.
Using volume + insider confirmation as a compound label dramatically reduces false positives
at the cost of a smaller positive class — acceptable once ≥ 6 months of Form 4 data exists.
