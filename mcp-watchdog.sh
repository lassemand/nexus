#!/usr/bin/env bash
# mcp-watchdog.sh — tails /tmp/mcp-notify.jsonl and invokes claude --print
# for each new notification, bypassing the broken channel mechanism.
#
# Usage: ./mcp-watchdog.sh
# Keep running in a tmux pane alongside the hookdeck listeners.

SESSION_ID="fae871a8-8238-440d-8666-504beb303cdf"
QUEUE="/tmp/mcp-notify.jsonl"
CURSOR="/tmp/mcp-notify.cursor"

# Touch queue file so tail doesn't error on first run
touch "$QUEUE"

# Track how many lines we've processed
PROCESSED=0
if [[ -f "$CURSOR" ]]; then
    PROCESSED=$(cat "$CURSOR")
fi

echo "[watchdog] starting — session $SESSION_ID, queue $QUEUE, processed=$PROCESSED"

while true; do
    TOTAL=$(wc -l < "$QUEUE")
    if (( TOTAL > PROCESSED )); then
        # Process each new line
        while (( PROCESSED < TOTAL )); do
            PROCESSED=$((PROCESSED + 1))
            LINE=$(sed -n "${PROCESSED}p" "$QUEUE")
            EVENT=$(echo "$LINE" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('event',''))" 2>/dev/null)
            CONTENT=$(echo "$LINE" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('content',''))" 2>/dev/null)

            echo "[watchdog] new event: $EVENT"

            # Format the prompt depending on event type
            if [[ "$EVENT" == "issue_todo" ]]; then
                PROMPT="<channel source=\"webhook\" event=\"issue_todo\">${CONTENT}</channel>"
            elif [[ "$EVENT" == "pr_review_comment" ]]; then
                PROMPT="<channel source=\"webhook\" event=\"pr_review_comment\">${CONTENT}</channel>"
            elif [[ "$EVENT" == "pr_comment" ]]; then
                PROMPT="<channel source=\"webhook\" event=\"pr_comment\">${CONTENT}</channel>"
            else
                PROMPT="<channel source=\"webhook\" event=\"${EVENT}\">${CONTENT}</channel>"
            fi

            echo "[watchdog] invoking claude --print for event $EVENT"
            claude \
                --resume "$SESSION_ID" \
                --agent=backend \
                --dangerously-skip-permissions \
                --print "$PROMPT" &

            echo $PROCESSED > "$CURSOR"
        done
    fi
    sleep 2
done
