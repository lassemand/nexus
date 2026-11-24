#!/usr/bin/env bash
# Downloads one past MFN report release as an accuracy fixture.
#
# Usage: analyst/tests/accuracy/fetch.sh <mfn_slug> <news_id> <fixture_dir> <ticker> <exchange_mic> <country> <sector>
#
# Writes <fixture_dir>/release.txt exactly as the analyst's runtime sees a release
# (the title, a blank line, MFN's content.text) and <fixture_dir>/expected.json with
# everything but the hand-checked metrics: the company, the release, and the
# company's 8 latest report releases published before it (the input of a dry
# preparation). The metrics are added by hand afterwards (README.md). An existing
# expected.json keeps its metrics. Needs curl and jq.
set -euo pipefail

[[ $# -eq 7 ]] || { sed -n '2,12p' "$0" >&2; exit 2; }
slug="$1" id="$2" dir="$3" ticker="$4" mic="$5" country="$6" sector="$7"
MFN_BASE="${MFN_BASE:-https://mfn.se}"
pages="${FEED_PAGES:-10}"

mkdir -p "$dir"
items="$(mktemp)"
item_file="$(mktemp)"
trap 'rm -f "$items" "$item_file"' EXIT
is_report='(.properties.tags // []) | any(. == "sub:report:interim" or . == "sub:report:annual" or startswith("sub:report:interim:"))'
url="$MFN_BASE/all/a/${slug}.json?limit=100"
for _ in $(seq 1 "$pages"); do
  [[ -n "$url" ]] || break
  page="$(curl -fsS -g --retry 3 --max-time 60 -A 'nexus-analyst-accuracy' "$url")"
  jq -c '.items // [] | .[]' <<<"$page" >> "$items"
  url="$(jq -r '.next_url // empty' <<<"$page")"
  at="$(jq -r --arg id "$id" 'select(.news_id == $id) | .content.publish_date' "$items" | head -n1)"
  if [[ -n "$at" ]]; then
    older="$(jq -s --arg at "$at" "map(select($is_report) | select(.content.publish_date < \$at)) | length" "$items")"
    [[ "$older" -ge 16 ]] && break
  fi
done

jq -c --arg id "$id" 'select(.news_id == $id)' "$items" | head -n1 > "$item_file"
[[ -s "$item_file" ]] || { echo "news_id $id not found in the feed of $slug" >&2; exit 1; }
jq -j '(.content.title // "") + "\n\n" + (.content.text // "")' "$item_file" > "$dir/release.txt"

published="$(jq -r '.content.publish_date' "$item_file")"
previous="$(jq -s -c --arg at "$published" '
  map(select('"$is_report"'))
  | map(select(.content.publish_date < $at))
  | unique_by(.news_id)
  | sort_by(.content.publish_date) | reverse | .[:8]
  | map({news_id, lang: (.properties.lang // "xx"), title: .content.title, published_at: .content.publish_date, url})' "$items")"

metrics='[]'
if [[ -f "$dir/expected.json" ]]; then
  metrics="$(jq -c '.metrics // []' "$dir/expected.json")"
fi

jq -n \
  --slurpfile items "$item_file" --argjson previous "$previous" --argjson metrics "$metrics" \
  --arg ticker "$ticker" --arg mic "$mic" --arg country "$country" --arg sector "$sector" '
  $items[0] as $item
  | {
    country: $country,
    sector: $sector,
    company: {ticker: $ticker, exchange_mic: $mic, name: $item.author.name, mfn_slug: $item.author.slug},
    release: {
      news_id: $item.news_id,
      lang: ($item.properties.lang // "xx"),
      title: $item.content.title,
      published_at: $item.content.publish_date,
      url: $item.url,
      kind: (if (($item.properties.tags // []) | any(. == "sub:report:annual" or . == "sub:report:interim:q4")) then "annual" else "interim" end)
    },
    previous_reports: $previous,
    metrics: $metrics
  }' > "$dir/expected.json"
echo "wrote $dir ($(jq '.previous_reports | length' "$dir/expected.json") previous reports)" >&2
