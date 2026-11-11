#!/usr/bin/env bash
# Downloads MFN interim-report releases for the kernel benchmark.
#
# Usage: verdict/fetch-corpus.sh [OUT_DIR] [MAX_RELEASES]
#   OUT_DIR       default verdict/corpus (gitignored; never commit the corpus)
#   MAX_RELEASES  default 2000
#
# Each release is written as <lang>__<slug>__<id>.txt: the title, a blank line,
# then content.text. Existing files are kept, so a rerun only adds new ones.
# Needs curl and jq.
set -euo pipefail

out_dir="${1:-$(dirname "$0")/corpus}"
max="${2:-2000}"
page=100
feed="https://mfn.se/all/s/nordic.json"
filter='(and(or(.properties.tags@>["sub:report:interim"])))'

command -v curl >/dev/null || { echo "curl is required" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }
mkdir -p "$out_dir"

written=0
offset=0
while (( offset < max )); do
  body="$(curl -fsS -G --retry 3 "$feed" \
    --data-urlencode "limit=$page" \
    --data-urlencode "offset=$offset" \
    --data-urlencode "filter=$filter")"
  count="$(jq '.items // [] | length' <<<"$body")"
  (( count == 0 )) && break
  while IFS=$'\t' read -r lang slug id content; do
    [[ -n "$content" ]] || continue
    slug="$(tr -c 'a-zA-Z0-9-' '_' <<<"$slug" | sed 's/_$//')"
    file="$out_dir/${lang}__${slug}__${id}.txt"
    [[ -e "$file" ]] && continue
    base64 -d <<<"$content" >"$file"
    written=$((written + 1))
  done < <(jq -r '.items // [] | .[]
    | select((.content.text // "") != "")
    | [ (.properties.lang // "xx"),
        (.author.slug // "unknown"),
        .news_id,
        (((.content.title // "") + "\n\n" + .content.text) | @base64) ]
    | @tsv' <<<"$body")
  offset=$((offset + count))
  echo "offset $offset, $written new" >&2
  (( count < page )) && break
done
echo "wrote $written releases to $out_dir" >&2
