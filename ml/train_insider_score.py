"""
Train an XGBoost insider-trading score model and export to ONNX.

Usage:
    python ml/train_insider_score.py \
        --database-url postgresql://nexus:<pw>@localhost:5432/backtest \
        [--label-threshold 0.08] \
        [--min-rows 50] \
        [--output-path ml/insider_score.onnx]

Feature order (must match scorer.rs FEATURE_NAMES):
    car_20d, car_10d, car_5d, mean_abvol_20d, max_abvol_20d,
    realized_vol_20d, price_momentum_20d, vol_trend_slope
"""

import argparse
import os
import sys

import numpy as np
import pandas as pd
from sklearn.metrics import classification_report, roc_auc_score
from sqlalchemy import create_engine
import xgboost as xgb
from skl2onnx import convert_sklearn
from skl2onnx.common.data_types import FloatTensorType

# ── Feature order — must match scorer.rs FEATURE_NAMES ────────────────────────

FEATURE_NAMES = [
    "car_20d",
    "car_10d",
    "car_5d",
    "mean_abvol_20d",
    "max_abvol_20d",
    "realized_vol_20d",
    "price_momentum_20d",
    "vol_trend_slope",
]

# ── Args ───────────────────────────────────────────────────────────────────────

parser = argparse.ArgumentParser(description="Train XGBoost insider-score model")
parser.add_argument("--database-url", required=True, help="Postgres connection string")
parser.add_argument("--label-threshold", type=float, default=0.08,
                    help="post_ar_1d threshold for positive label (default: 0.08)")
parser.add_argument("--min-rows", type=int, default=50,
                    help="Minimum labeled rows required to train (default: 50)")
parser.add_argument("--output-path", default="ml/insider_score.onnx",
                    help="Where to write the ONNX model (default: ml/insider_score.onnx)")
args = parser.parse_args()

# ── Load data ─────────────────────────────────────────────────────────────────

engine = create_engine(args.database_url)
df = pd.read_sql(
    """
    SELECT event_date, post_ar_1d, {features}
    FROM event_signals
    WHERE truth_labeled_at IS NOT NULL
      AND pre_event_bars >= 20
    ORDER BY event_date
    """.format(features=", ".join(FEATURE_NAMES)),
    engine,
)

df = df.dropna(subset=FEATURE_NAMES + ["post_ar_1d"])

if len(df) < args.min_rows:
    print(f"ERROR: only {len(df)} labeled rows — need at least {args.min_rows} to train.")
    sys.exit(1)

print(f"Loaded {len(df)} labeled rows (after dropping NULLs).")

# ── Labels ────────────────────────────────────────────────────────────────────

y = (df["post_ar_1d"] > args.label_threshold).astype(int)
X = df[FEATURE_NAMES].astype(np.float32)

print(f"Positive class: {y.sum()} / {len(y)} ({100 * y.mean():.1f}%)")

# ── Time-ordered train/test split (no shuffle — avoids look-ahead bias) ───────

split = int(len(X) * 0.8)
X_train, X_test = X.iloc[:split], X.iloc[split:]
y_train, y_test = y.iloc[:split], y.iloc[split:]

print(f"Train: {len(X_train)} rows | Test: {len(X_test)} rows")

# ── Train ─────────────────────────────────────────────────────────────────────

model = xgb.XGBClassifier(
    n_estimators=200,
    max_depth=4,
    learning_rate=0.05,
    subsample=0.8,
    colsample_bytree=0.8,
    eval_metric="auc",
    use_label_encoder=False,
    random_state=42,
)
model.fit(X_train, y_train, eval_set=[(X_test, y_test)], verbose=False)

# ── Evaluate ──────────────────────────────────────────────────────────────────

y_prob = model.predict_proba(X_test)[:, 1]
y_pred = (y_prob >= 0.5).astype(int)
auc = roc_auc_score(y_test, y_prob)

print(f"\nTest AUC: {auc:.4f}")
print(classification_report(y_test, y_pred, target_names=["no-signal", "signal"]))

# ── Export to ONNX ────────────────────────────────────────────────────────────

initial_type = [("float_input", FloatTensorType([None, len(FEATURE_NAMES)]))]
onnx_model = convert_sklearn(model, initial_types=initial_type)

os.makedirs(os.path.dirname(args.output_path) or ".", exist_ok=True)
with open(args.output_path, "wb") as f:
    f.write(onnx_model.SerializeToString())

print(f"\nModel exported to {args.output_path}")
