#!/bin/bash

RUN_FILE="/run/bwmon.state"
STATE_DIR="/run/user/$UID/bwmon"
STATE_FILE="${STATE_DIR}/bwmon.state"

# Ensure our user-specific state directory exists
if [[ ! -d "$STATE_DIR" ]]; then
  mkdir -p "$STATE_DIR" || exit 1
fi

if [[ ! -r "$RUN_FILE" ]]; then
  echo "⚠ No Data | color=red"
  echo "---"
  echo "Publisher service not running or $RUN_FILE missing."
  exit 0
fi

TOTAL_RX_B=0
TOTAL_TX_B=0
DROPDOWN_OUTPUT=""
NEW_STATE_BUFFER=""

format_speed() {
  local bytes=$1
  local out_var=$2
  local result

  if [ "$bytes" -lt 1048576 ]; then
    result="$(( bytes / 1024 )) KiB/s"
  elif [ "$bytes" -lt 1073741824 ]; then
    result="$(( bytes / 1048576 )).$(( (bytes % 1048576) * 10 / 1048576 )) MiB/s"
  else
    result="$(( bytes / 1073741824 )).$(( (bytes % 1073741824) * 10 / 1073741824 )) GiB/s"
  fi

  printf -v "$out_var" "%s" "$result"
}

# Load all previous states into memory at once
declare -A PREV_RX_MAP PREV_TX_MAP
if [[ -r "$STATE_FILE" ]]; then
  while read -r p_netns p_device p_rx p_tx; do
    PREV_RX_MAP["${p_netns}_${p_device}"]=$p_rx
    PREV_TX_MAP["${p_netns}_${p_device}"]=$p_tx
  done < "$STATE_FILE"
fi

# Read the live data line by line
while read -r netns device RX TX; do
  [[ -z "$netns" ]] && continue

  # Validate RX and TX are strictly numbers
  if [[ ! "$RX" =~ ^[0-9]+$ ]] || [[ ! "$TX" =~ ^[0-9]+$ ]]; then
    continue
  fi

  KEY="${netns}_${device}"

  # Retrieve previous state from memory, default to 0
  PREV_RX=${PREV_RX_MAP[$KEY]:-0}
  PREV_TX=${PREV_TX_MAP[$KEY]:-0}

  # Calculate Speeds
  RX_DIFF=$((RX - PREV_RX))
  TX_DIFF=$((TX - PREV_TX))

  [ "$RX_DIFF" -lt 0 ] && RX_DIFF=0
  [ "$TX_DIFF" -lt 0 ] && TX_DIFF=0

  # Append to the new state buffer to write to disk later
  NEW_STATE_BUFFER+="$netns $device $RX $TX\n"

  # Add raw bytes to global totals
  TOTAL_RX_B=$((TOTAL_RX_B + RX_DIFF))
  TOTAL_TX_B=$((TOTAL_TX_B + TX_DIFF))

  # Format individual speeds directly into variables
  format_speed "$RX_DIFF" FMT_RX_SPEED
  format_speed "$TX_DIFF" FMT_TX_SPEED

  case "$device" in
    wg*|tun*|tap*) ICON="🔒" ;;  # WireGuard / VPN
    en*|eth*)      ICON="🔌" ;;  # Ethernet
    wl*)           ICON="📡" ;;  # Wi-Fi
    lo)            ICON="🔄" ;;  # Loopback
    *)             ICON="🌐" ;;  # Default / Other
  esac

  INDENT="&#160;&#160;&#160;&#160;"
  DROPDOWN_OUTPUT+="<span>$ICON [$netns] <b>$device</b></span> | useMarkup=true\n"
  DROPDOWN_OUTPUT+="${INDENT}<span font_family='monospace'>↓ ${FMT_RX_SPEED}  ↑ ${FMT_TX_SPEED}</span> | useMarkup=true\n"
  DROPDOWN_OUTPUT+="${INDENT}<span font_family='monospace'>RX: $((RX / 1048576)) MiB  TX: $((TX / 1048576)) MiB</span> | useMarkup=true\n"

done < "$RUN_FILE"

# Save all states to disk
printf -v NEW_STATE_OUTPUT "%b" "$NEW_STATE_BUFFER"
echo "$NEW_STATE_OUTPUT" > "$STATE_FILE"

# Format total speeds
format_speed "$TOTAL_RX_B" FMT_TOT_RX
format_speed "$TOTAL_TX_B" FMT_TOT_TX

# --- ARGOS OUTPUT ---
echo "↓ ${FMT_TOT_RX}  ↑ ${FMT_TOT_TX} | font=monospace"
echo "---"
echo -e "$DROPDOWN_OUTPUT"
