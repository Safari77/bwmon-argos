#!/bin/bash

CONFIG="/etc/bwmon-devices"
OUTFILE="/run/bwmon.state"
TMPFILE="${OUTFILE}.tmp"

if [[ ! -f "$CONFIG" ]]; then
    echo "Error: $CONFIG not found."
    exit 1
fi
if [[ ! -d /run ]]; then
    echo /run does not exist
    exit 1
fi

while true; do
    # Delete and empty the temp file for this tick
    rm -f "$TMPFILE"
    > "$TMPFILE"

    while read -r netns device; do
        # Skip empty lines or comments
        [[ -z "$netns" || "$netns" == \#* ]] && continue

        if [ "$netns" = "-" ]; then
            # Init namespace
            read -r RX < "/sys/class/net/$device/statistics/rx_bytes" 2>/dev/null || RX=0
            read -r TX < "/sys/class/net/$device/statistics/tx_bytes" 2>/dev/null || TX=0
        else
            # Specific namespace (Read both files in one fork using an array)
            STATS=($(ip netns exec "$netns" /bin/cat /sys/class/net/"$device"/statistics/rx_bytes /sys/class/net/"$device"/statistics/tx_bytes 2>/dev/null))
            RX=${STATS[0]:-0}
            TX=${STATS[1]:-0}
        fi

        # Append to temp file
        echo "$netns $device ${RX} ${TX}" >> "$TMPFILE"

    done < "$CONFIG"

    # Atomic swap guarantees the Argos script never reads a half-written file
    mv -f "$TMPFILE" "$OUTFILE"

    sleep 1
done
